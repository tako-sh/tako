use super::*;
use std::io::{IsTerminal, Read};
use tako_network::api::{RpcRequest, RpcResponse, SignedRequest};
use tako_network::invitation::{ClaimInvitation, CreateInvitation, Invitation, InvitationToken};
use unicode_width::UnicodeWidthStr;
use zeroize::Zeroizing;

#[derive(Subcommand)]
pub enum ClientCommands {
    /// Invite a client. Without --deploy the client gets full access. Adding a
    /// revoked client's name brings it back; adding an active client's name
    /// replaces its key
    Add {
        /// Client name, such as `alice-laptop` or `github-ci`
        name: tako_network::DeviceName,
        /// Restrict the client to deploying this app/environment; repeat for more
        #[arg(long, value_name = "APP/ENV")]
        deploy: Vec<tako_network::AppEnv>,
        /// Invitation validity in seconds (at most one day)
        #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(60..=86400))]
        expires_in: u64,
        /// Replace an active client's key without asking
        #[arg(long, short = 'y')]
        yes: bool,
        /// Print a client identity for TAKO_CLIENT_IDENTITY instead of an
        /// invitation, for machines that keep no state, such as CI
        #[arg(long)]
        identity: bool,
    },
    /// Refresh and list clients in the network
    #[command(visible_alias = "ls")]
    List {
        /// Also list revoked, expired, and replaced keys
        #[arg(long)]
        all: bool,
    },
    /// Revoke a client's active key. Clients it invited keep their access
    /// unless --include-invited
    #[command(visible_aliases = ["rm", "remove"])]
    Revoke {
        /// Client name or device ID
        client: String,
        /// Also revoke every client it invited, and the clients those invited
        #[arg(long)]
        include_invited: bool,
        /// Revoke with --include-invited without asking
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

/// One admitted client key, as `client list` shows it.
struct ClientRow<'a> {
    name: String,
    member: &'a tako_network::Member,
    status: tako_network::MemberStatus,
    invited_by: String,
}

impl ClientRow<'_> {
    fn access(&self) -> String {
        match self.member.permissions.deploy_scopes() {
            None => "full".into(),
            Some(scopes) => format!(
                "deploy {}",
                scopes
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "device": self.member.device_id,
            "status": self.status,
            "access": if self.member.permissions.is_full() { "full" } else { "deploy" },
            "scopes": self.member.permissions.deploy_scopes(),
            "invited_by": self.invited_by,
            "created_at": self.member.created_at,
            "expires_at": self.member.expires_at,
        })
    }
}

/// Every admitted client key in this view, oldest first.
fn client_rows(snapshot: &tako_network::RegistrySnapshot, now: u64) -> Result<Vec<ClientRow<'_>>> {
    let names = snapshot.client_names();
    let statuses = snapshot.member_statuses(now)?;
    let label = |id: &tako_network::DeviceId| -> String {
        names.get(id).cloned().unwrap_or_else(|| {
            statuses
                .get(id)
                .map(|(member, _)| member.request.name.to_string())
                .unwrap_or_else(|| id.to_string())
        })
    };
    let mut rows: Vec<_> = statuses
        .iter()
        .filter(|(_, (member, _))| matches!(member.kind, MemberKind::Laptop))
        .map(|(id, (member, status))| ClientRow {
            name: label(id),
            member,
            status: *status,
            invited_by: match &member.approved_by {
                Some(approver) => label(approver),
                None if member.independent_root => "host recovery".into(),
                None => "network creator".into(),
            },
        })
        .collect();
    rows.sort_by(|a, b| {
        (a.member.created_at, &a.member.device_id).cmp(&(b.member.created_at, &b.member.device_id))
    });
    Ok(rows)
}

/// The clients that `device` invited and that still hold a key. They keep their
/// access when `device` is revoked. A key in conflict with another of the same
/// name is listed too, marked as such: it becomes active if that key is revoked.
fn invited_by<'a>(rows: &[ClientRow<'a>], device: &tako_network::DeviceId) -> Vec<String> {
    rows.iter()
        .filter(|row| row.member.approved_by.as_ref() == Some(device))
        .filter_map(|row| match row.status {
            tako_network::MemberStatus::Active => Some(row.name.clone()),
            tako_network::MemberStatus::Conflict => Some(format!("{} (conflicting key)", row.name)),
            _ => None,
        })
        .collect()
}

/// The keys client `name` still holds: its active key and any admitted
/// concurrently that are in conflict with it. Once the active key is revoked one
/// of the conflicting keys would take its place, so a replacement revokes all.
fn held_keys<'r, 'a>(rows: &'r [ClientRow<'a>], name: &str) -> Vec<&'r ClientRow<'a>> {
    rows.iter()
        .filter(|row| {
            row.name.eq_ignore_ascii_case(name)
                && matches!(
                    row.status,
                    tako_network::MemberStatus::Active | tako_network::MemberStatus::Conflict
                )
        })
        .collect()
}

/// Every client key that `root` invited, directly or through the clients it
/// invited, that still holds access. Servers a client added are not clients it
/// invited.
fn invited_subtree<'r, 'a>(
    rows: &'r [ClientRow<'a>],
    root: &tako_network::DeviceId,
) -> Vec<&'r ClientRow<'a>> {
    let mut visited = vec![root.clone()];
    let mut frontier = vec![root.clone()];
    let mut holding = Vec::new();
    while let Some(parent) = frontier.pop() {
        for row in rows
            .iter()
            .filter(|row| row.member.approved_by.as_ref() == Some(&parent))
        {
            if visited.contains(&row.member.device_id) {
                continue;
            }
            visited.push(row.member.device_id.clone());
            // A revoked or replaced key's invitees are still in the subtree.
            frontier.push(row.member.device_id.clone());
            if matches!(
                row.status,
                tako_network::MemberStatus::Active | tako_network::MemberStatus::Conflict
            ) {
                holding.push(row);
            }
        }
    }
    holding
}

/// Why revoking `targets` together must not start, if it must not.
fn refuse_revoking_together(
    rows: &[ClientRow<'_>],
    targets: &[&ClientRow<'_>],
    own: &tako_network::DeviceId,
) -> Option<String> {
    if let Some(row) = targets.iter().find(|row| &row.member.device_id == own) {
        return Some(format!(
            "This device is client {}, which would be revoked too. Revoke them from another full client.",
            row.name
        ));
    }
    let revoked = |row: &ClientRow<'_>| {
        targets
            .iter()
            .any(|target| target.member.device_id == row.member.device_id)
    };
    let full_left = rows.iter().any(|row| {
        row.status == tako_network::MemberStatus::Active
            && row.member.permissions.is_full()
            && !revoked(row)
    });
    (!full_left).then(|| {
        "Revoking these clients would leave the network without a full client. Invite another full client first.".to_string()
    })
}

fn own_device(profile: &Profile) -> Result<tako_network::DeviceId> {
    Ok(profile
        .request()
        .device_id_for(&profile.snapshot().network.id)?)
}

fn invited_hint(name: &str, invited: &[String]) {
    if !invited.is_empty() {
        output::hint(&format!(
            "Clients that {name} invited keep their access: {}. Revoke them with `tako network client revoke <name>` if needed.",
            invited.join(", ")
        ));
    }
}

pub(super) fn run(command: ClientCommands) -> Result<()> {
    match command {
        ClientCommands::Add {
            name,
            deploy,
            expires_in,
            yes,
            identity,
        } => add(&name, deploy, expires_in, yes, identity),
        ClientCommands::List { all } => {
            let alias = network::resolve_network(None)?;
            let profile = if output::is_dry_run() {
                network::load_profile(&alias)?
            } else {
                network::state::pull(&alias, None)?.0
            };
            let rows = client_rows(profile.snapshot(), network::now()?)?;
            let rows: Vec<_> = rows
                .into_iter()
                .filter(|row| all || row.status == tako_network::MemberStatus::Active)
                .collect();
            if output::is_json() {
                return output::json_result(serde_json::json!({
                    "ok": true,
                    "command": "network client list",
                    "network": alias.alias,
                    "clients": rows.iter().map(ClientRow::json).collect::<Vec<_>>(),
                }));
            }
            if rows.is_empty() {
                output::warning("No clients");
                tracing::warn!("No clients");
                return Ok(());
            }
            let width = |header: &str, value: &dyn Fn(&ClientRow) -> String| {
                rows.iter()
                    .map(|row| value(row).width())
                    .chain(std::iter::once(header.width()))
                    .max()
                    .unwrap_or_default()
            };
            let name_width = width("NAME", &|row| row.name.clone());
            let access_width = width("ACCESS", &|row| row.access());
            let invited_width = width("INVITED BY", &|row| row.invited_by.clone());
            let pad = |value: &str, width: usize| {
                format!("{value}{}", " ".repeat(width.saturating_sub(value.width())))
            };
            output::heading("Clients");
            let header = format!(
                "{}  {}  {}{}",
                pad("NAME", name_width),
                pad("ACCESS", access_width),
                pad("INVITED BY", invited_width),
                if all { "  STATUS" } else { "" }
            );
            output::info(header.trim_end());
            for row in &rows {
                let status = serde_json::to_value(row.status)?
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let line = format!(
                    "{}  {}  {}{}",
                    pad(&row.name, name_width),
                    pad(&row.access(), access_width),
                    pad(&row.invited_by, invited_width),
                    if all {
                        format!("  {status}")
                    } else {
                        String::new()
                    }
                );
                output::info(line.trim_end());
                tracing::info!(
                    "{} access={} invited_by={} status={status} device={}",
                    row.name,
                    row.access(),
                    row.invited_by,
                    row.member.device_id
                );
            }
            Ok(())
        }
        ClientCommands::Revoke {
            client,
            include_invited,
            yes,
        } => {
            if include_invited {
                revoke_with_invited(&client, yes)
            } else {
                revoke(&client)
            }
        }
    }
}

fn add(
    name: &tako_network::DeviceName,
    deploy: Vec<tako_network::AppEnv>,
    expires_in: u64,
    yes: bool,
    identity: bool,
) -> Result<()> {
    let alias = network::resolve_network(None)?;
    if output::is_dry_run() {
        return finish(
            "Would create a client invitation",
            serde_json::json!({"dry_run":true,"network":alias.alias,"client":name}),
        );
    }
    let full = deploy.is_empty();
    let profile = network::state::pull(&alias, None)?.0;
    let now = network::now()?;
    let own = own_device(&profile)?;
    let rows = client_rows(profile.snapshot(), now)?;
    let same_name = |row: &&ClientRow| row.name.eq_ignore_ascii_case(name.as_str());
    let active = rows
        .iter()
        .find(|row| same_name(row) && row.status == tako_network::MemberStatus::Active);
    let replacing = held_keys(&rows, name.as_str());
    let known = rows.iter().any(|row| same_name(&row));
    let mut invited = Vec::new();
    if !replacing.is_empty() {
        if replacing.iter().any(|row| row.member.device_id == own) {
            return Err(format!(
                "This device is client {name}. Replace its key from another full client."
            )
            .into());
        }
        for row in &replacing {
            for child in invited_by(&rows, &row.member.device_id) {
                if !invited.contains(&child) {
                    invited.push(child);
                }
            }
        }
        if !yes {
            if !output::is_interactive() {
                return Err(
                    format!("Client {name} is active. Pass --yes to replace its key.").into(),
                );
            }
            let prompt = if replacing.len() == 1 {
                format!(
                    "{} is active. Replace its key? The current key stops working.",
                    output::strong(name.as_str())
                )
            } else {
                format!(
                    "{} holds {} keys. Replace them? They all stop working.",
                    output::strong(name.as_str()),
                    replacing.len()
                )
            };
            if !output::confirm(&prompt, false)? {
                output::operation_cancelled();
                return Ok(());
            }
        }
    }
    let replaced = active.map(|row| row.member.device_id.clone());
    let conflicting: Vec<tako_network::DeviceId> = replacing
        .iter()
        .map(|row| row.member.device_id.clone())
        .filter(|id| Some(id) != replaced.as_ref())
        .collect();
    let server = network::client::available_server(&profile)?;
    let info = network::server_members(&profile)?
        .get(&server)
        .and_then(|m| match &m.kind {
            MemberKind::Server(info) => Some(info.clone()),
            _ => None,
        })
        .ok_or("Invitation issuer is unavailable")?;
    let token = InvitationToken::random()?;
    let id = tako_network::RequestId::random()?;
    let expires_at = network::now()? + expires_in;
    let grant = CreateInvitation {
        id: id.clone(),
        token_hash: token.hash()?,
        permissions: super::access(deploy),
        expires_at,
        member_expires_at: None,
        client: name.clone(),
    };
    match network::client::call(
        &profile,
        &server,
        RpcRequest::CreateInvitation(Box::new(grant)),
    )? {
        RpcResponse::InvitationCreated { id: returned } if returned == id => (),
        _ => return Err("Unexpected invitation response".into()),
    }
    // The old keys stop working before anyone sees the new invitation.
    for device in replaced.iter().chain(&conflicting) {
        network::revocation::run(&alias, &device.to_string())?;
    }
    let invitation = Invitation {
        version: tako_network::invitation::INVITATION_VERSION,
        network: profile.snapshot().network.clone(),
        issuer: server,
        server: info,
        id,
        token,
        expires_at,
        client: name.clone(),
    };
    if identity {
        return print_identity(
            &alias,
            name,
            &invitation,
            full,
            replaced,
            &conflicting,
            invited,
            known,
        );
    }
    let invitation = invitation.encode()?;
    if output::is_json() {
        output::json_result(serde_json::json!({
            "network": alias.alias,
            "client": name,
            "invitation": invitation.as_str(),
            "expires_at": expires_at,
            "replaced": replaced,
            "also_revoked": conflicting,
            "invited": invited,
        }))?;
    } else {
        // Deliberate credential output; never put it in tracing or progress logs.
        println!("{}", invitation.as_str());
        let message = if replaced.is_some() || !conflicting.is_empty() {
            format!("Revoked {name}'s key and created an invitation for a new one")
        } else if known {
            format!("Created an invitation to bring back {name}")
        } else {
            format!("Created an invitation for {name}")
        };
        output::success(&message);
    }
    invited_hint(name.as_str(), &invited);
    output::hint(
        "On the new machine, run `tako network join` and paste the invitation. It can enroll one client.",
    );
    Ok(())
}

/// Redeem `invitation` as a new device in a private temporary directory and
/// print that device as a client identity. Its keys never reach the Tako home.
fn print_identity(
    alias: &crate::config::LocalNetwork,
    name: &tako_network::DeviceName,
    invitation: &Invitation,
    full: bool,
    replaced: Option<tako_network::DeviceId>,
    conflicting: &[tako_network::DeviceId],
    invited: Vec<String>,
    known: bool,
) -> Result<()> {
    let identity = {
        let _temporary = network::identity::TemporaryState::new()?;
        let joined = redeem(invitation, &alias.alias)?;
        // Admission answers the server's key check with the new device's keys.
        network::client::refresh(&joined)?;
        network::identity::encode(&joined)?
    };
    if full {
        let warning = format!(
            "{name} has full access. Anyone holding this identity can read every secret and manage the network; use --deploy app/env for CI."
        );
        output::warning(&warning);
        tracing::warn!("{warning}");
    }
    if output::is_json() {
        output::json_result(serde_json::json!({
            "network": alias.alias,
            "client": name,
            "identity": identity.as_str(),
            "replaced": replaced,
            "also_revoked": conflicting,
            "invited": invited,
        }))?;
    } else {
        // Deliberate credential output on stdout, so it can be piped into a
        // secret store; never put it in tracing or progress logs.
        println!("{}", identity.as_str());
        let message = if replaced.is_some() || !conflicting.is_empty() {
            format!("Revoked {name}'s key and created an identity with a new one")
        } else if known {
            format!("Created an identity to bring back {name}")
        } else {
            format!("Created an identity for {name}")
        };
        output::success(&message);
    }
    invited_hint(name.as_str(), &invited);
    output::hint(&format!(
        "Store it as a secret named {} where the client runs. `tako network client revoke {name}` stops it.",
        network::identity::ENV
    ));
    Ok(())
}

fn revoke(client: &str) -> Result<()> {
    let alias = network::resolve_network(None)?;
    let profile = if output::is_dry_run() {
        network::load_profile(&alias)?
    } else {
        network::client::refresh(&alias)?
    };
    let now = network::now()?;
    let rows = client_rows(profile.snapshot(), now)?;
    // A device ID names one key; a name names the client's active key.
    let target = rows
        .iter()
        .find(|row| row.member.device_id.to_string() == client)
        .or_else(|| {
            rows.iter().find(|row| {
                row.status == tako_network::MemberStatus::Active
                    && row.name.eq_ignore_ascii_case(client)
            })
        })
        .ok_or("No active client has this name or device ID")?;
    if target.member.device_id == own_device(&profile)? {
        return Err("This device cannot revoke itself. Revoke it from another full client.".into());
    }
    let invited = invited_by(&rows, &target.member.device_id);
    let name = target.name.clone();
    let device = target.member.device_id.to_string();
    if output::is_dry_run() {
        network::revocation::project(&alias, &device)?;
        return finish(
            "Would revoke the client",
            serde_json::json!({"dry_run": true, "network": alias.alias, "client": name, "device": device, "invited": invited}),
        );
    }
    let revocation = network::revocation::run(&alias, &device)?;
    if let Err(error) = network::state::pull(&alias, None) {
        output::hint(&format!(
            "Membership changed; refresh network state later with `tako network sync`: {error}"
        ));
    }
    finish(
        &format!("Revoked {name}"),
        serde_json::json!({"network": alias.alias, "client": name, "revocation": revocation, "invited": invited, "enforcement": "pending"}),
    )?;
    invited_hint(&name, &invited);
    output::hint(
        "Servers enforce the revocation as it reaches them. Unreachable servers remain pending.",
    );
    Ok(())
}

/// Revoke a client together with every client it invited.
fn revoke_with_invited(client: &str, yes: bool) -> Result<()> {
    let alias = network::resolve_network(None)?;
    let profile = if output::is_dry_run() {
        network::load_profile(&alias)?
    } else {
        network::client::refresh(&alias)?
    };
    let now = network::now()?;
    let rows = client_rows(profile.snapshot(), now)?;
    let target = rows
        .iter()
        .find(|row| row.member.device_id.to_string() == client)
        .or_else(|| {
            rows.iter().find(|row| {
                row.status == tako_network::MemberStatus::Active
                    && row.name.eq_ignore_ascii_case(client)
            })
        })
        .ok_or("No active client has this name or device ID")?;
    let mut targets = vec![target];
    targets.extend(invited_subtree(&rows, &target.member.device_id));
    if let Some(reason) = refuse_revoking_together(&rows, &targets, &own_device(&profile)?) {
        return Err(reason.into());
    }
    let listed = |targets: &[&ClientRow<'_>]| {
        targets
            .iter()
            .map(|row| serde_json::json!({"client": row.name, "device": row.member.device_id}))
            .collect::<Vec<_>>()
    };
    let name = target.name.clone();
    let invited: Vec<String> = targets[1..].iter().map(|row| row.name.clone()).collect();
    if output::is_dry_run() {
        return finish(
            "Would revoke the client and the clients it invited",
            serde_json::json!({"dry_run": true, "network": alias.alias, "client": name, "revoked": listed(&targets)}),
        );
    }
    if !yes && !invited.is_empty() {
        if !output::is_interactive() {
            return Err(format!(
                "{name} invited {}. Pass --yes to revoke them all.",
                invited.join(", ")
            )
            .into());
        }
        let prompt = format!(
            "Revoke {} and the clients it invited: {}? Their keys stop working.",
            output::strong(&name),
            invited.join(", ")
        );
        if !output::confirm(&prompt, false)? {
            output::operation_cancelled();
            return Ok(());
        }
    }
    let mut revocations = Vec::new();
    for row in &targets {
        revocations.push(network::revocation::run(
            &alias,
            &row.member.device_id.to_string(),
        )?);
    }
    if let Err(error) = network::state::pull(&alias, None) {
        output::hint(&format!(
            "Membership changed; refresh network state later with `tako network sync`: {error}"
        ));
    }
    let message = if invited.is_empty() {
        format!("Revoked {name}; it invited no clients")
    } else {
        format!(
            "Revoked {name} and the clients it invited: {}",
            invited.join(", ")
        )
    };
    finish(
        &message,
        serde_json::json!({"network": alias.alias, "client": name, "revoked": listed(&targets), "revocations": revocations, "enforcement": "pending"}),
    )?;
    output::hint(
        "Servers enforce the revocations as they reach them. Unreachable servers remain pending.",
    );
    Ok(())
}

pub(super) fn join(from_stdin: bool, alias: Option<&str>) -> Result<()> {
    if output::is_dry_run() {
        return finish(
            "Would join a network with a new local identity",
            serde_json::json!({"dry_run":true}),
        );
    }
    let encoded = if from_stdin {
        if std::io::stdin().is_terminal() {
            return Err("Pipe the invitation or identity to --invitation-stdin, or use the protected prompt without this flag".into());
        }
        let mut value = Zeroizing::new(String::new());
        std::io::stdin()
            .take(64 * 1024 + 1)
            .read_to_string(&mut value)?;
        value
    } else {
        Zeroizing::new(output::password_field("Invitation or identity")?)
    };
    let identity = network::identity::is_identity(&encoded);
    let invitation = if identity {
        None
    } else {
        Some(Invitation::decode(&encoded)?)
    };
    let descriptor = match &invitation {
        Some(invitation) => invitation.network.clone(),
        None => network::identity::network_of(&encoded)?,
    };
    // A retried join keeps the alias this machine already has for the network.
    let networks = crate::config::LocalNetworks::load()?;
    let alias = match networks.networks.get(&descriptor.id.to_string()) {
        Some(existing) if alias.is_none_or(|alias| alias == existing.alias) => {
            existing.alias.clone()
        }
        Some(existing) => {
            return Err(format!(
                "This machine already has network {} as '{}'. Rename it with `tako network rename`.",
                descriptor.id, existing.alias
            )
            .into());
        }
        None => super::choose_alias(&networks, descriptor.name.as_str(), alias, "join")?,
    };
    let local = match &invitation {
        None => network::identity::store(&encoded, Some(&alias))?,
        Some(invitation) => redeem(invitation, &alias)?,
    };
    network::state::pull(&local, None)?;
    finish(
        &format!("Joined network {}", network::network_label(&local)),
        serde_json::to_value(&local)?,
    )
}

/// Enroll this device with an invitation and save its pending profile.
fn redeem(invitation: &Invitation, alias: &str) -> Result<crate::config::LocalNetwork> {
    let directory = crate::paths::network_state_dir()?
        .join("network-joins")
        .join(invitation.network.id.to_string());
    network::private_directory(&directory)?;
    let path = directory.join(format!("{}.json", invitation.id));
    let lock = network::private_file(&path.with_extension("lock"), true)?;
    lock.lock()?;
    let request: tako_network::EnrollmentRequest = if path.try_exists()? {
        network::read_json(&path)?
    } else {
        if invitation.expires_at <= network::now()? {
            return Err(
                "Invitation expired; create a new invitation on an authorized client".into(),
            );
        }
        let keys = LocalDeviceKeys::generate()?;
        let request = keys.enrollment_request(
            Some(invitation.network.id.clone()),
            DeviceKind::Laptop,
            invitation.client.clone(),
        )?;
        network::store_keys(&request.request_id, &keys)?;
        network::write_json(&path, &request, false)?;
        request
    };
    let keys = LocalDeviceKeys::load(&network::keys_path(&request.request_id)?)?;
    let local = crate::config::LocalNetworks::load()?;
    if let Some(existing) = local.networks.get(&invitation.network.id.to_string()) {
        let previous: Profile = network::read_json(&existing.profile_path()?)?;
        let known = network::server_members(&previous)?;
        if previous.snapshot().network != invitation.network
            || !known
                .get(&invitation.issuer)
                .is_some_and(|member| member.kind == MemberKind::Server(invitation.server.clone()))
        {
            return Err(
                "Invitation conflicts with this machine's existing network identity or server pins"
                    .into(),
            );
        }
    }
    Ok(
        if let Some(existing) = local.networks.values().find(|n| {
            n.id == invitation.network.id.to_string()
                && n.device
                    == request
                        .device_id_for(&invitation.network.id)
                        .map(|id| id.to_string())
                        .unwrap_or_default()
        }) {
            existing.clone()
        } else {
            let signed = SignedRequest::sign(
                &keys,
                invitation.network.id.clone(),
                invitation.issuer.clone(),
                DeviceKind::Laptop,
                network::now()?,
                RpcRequest::ClaimInvitation(Box::new(ClaimInvitation {
                    id: invitation.id.clone(),
                    token: invitation.token.clone(),
                    request: request.clone(),
                })),
            )?;
            let RpcResponse::Pending(pending) = network::client::send(&invitation.server, &signed)?
            else {
                return Err("Unexpected invitation claim response".into());
            };
            if pending.snapshot.network != invitation.network
                || pending.issuer != invitation.issuer
                || pending.request != request
                || pending.trusted_servers.get(&invitation.issuer) != Some(&invitation.server)
            {
                return Err(
                    "Invitation response does not match the pinned network and enrollment".into(),
                );
            }
            pending.validate_for(&keys, network::now()?)?;
            network::save_profile_as(&Profile::Pending { profile: pending }, Some(alias))?
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_a_client_takes_every_key_its_name_still_holds() {
        let fixture = crate::network::tests::fixture();
        let member = &fixture.profile.snapshot().members[0];
        let row = |name: &str, status| ClientRow {
            name: name.into(),
            member,
            status,
            invited_by: "-".into(),
        };
        use tako_network::MemberStatus::*;
        let rows = vec![
            row("ci", Active),
            row("CI", Conflict),
            row("ci", Revoked),
            row("ci", Replaced),
            row("ci", Expired),
            row("other", Active),
        ];
        let held: Vec<_> = held_keys(&rows, "ci")
            .into_iter()
            .map(|row| (row.name.as_str(), row.status))
            .collect();
        assert_eq!(held, [("ci", Active), ("CI", Conflict)]);
        assert!(held_keys(&rows, "nobody").is_empty());
    }

    #[test]
    fn revoke_notices_and_subtrees_include_conflicting_invitees() {
        use tako_network::MemberStatus::*;
        let fixture = crate::network::tests::fixture();
        let network = fixture.profile.snapshot().network.id.clone();
        let template = fixture.profile.snapshot().members[0].clone();
        let device = |name: &str| {
            tako_network::LocalDeviceKeys::generate()
                .unwrap()
                .enrollment_request(
                    Some(network.clone()),
                    tako_network::DeviceKind::Laptop,
                    name.parse().unwrap(),
                )
                .unwrap()
                .device_id_for(&network)
                .unwrap()
        };
        let owner = device("owner");
        let members: Vec<_> = [None, Some(&owner), Some(&owner), Some(&owner)]
            .into_iter()
            .enumerate()
            .map(|(index, by)| {
                let mut member = template.clone();
                member.device_id = if index == 0 {
                    owner.clone()
                } else {
                    device(&format!("invitee{index}"))
                };
                member.approved_by = by.cloned();
                member
            })
            .collect();
        let rows: Vec<_> = members
            .iter()
            .zip([Active, Active, Conflict, Revoked])
            .zip(["owner", "ci", "ci", "old"])
            .map(|((member, status), name)| ClientRow {
                name: name.into(),
                member,
                status,
                invited_by: "-".into(),
            })
            .collect();
        // The conflicting key keeps its approval and would become active, so
        // the notice names it; the revoked key holds nothing and is left out.
        assert_eq!(invited_by(&rows, &owner), ["ci", "ci (conflicting key)"]);
        let subtree: Vec<_> = invited_subtree(&rows, &owner)
            .into_iter()
            .map(|row| (row.name.as_str(), row.status))
            .collect();
        assert_eq!(subtree, [("ci", Active), ("ci", Conflict)]);
    }

    #[test]
    fn including_invited_revokes_the_whole_subtree_but_never_the_last_full_client() {
        use tako_network::MemberStatus::*;
        let fixture = crate::network::tests::fixture();
        let network = fixture.profile.snapshot().network.id.clone();
        let template = fixture.profile.snapshot().members[0].clone();
        let device = |name: &str| {
            tako_network::LocalDeviceKeys::generate()
                .unwrap()
                .enrollment_request(
                    Some(network.clone()),
                    tako_network::DeviceKind::Laptop,
                    name.parse().unwrap(),
                )
                .unwrap()
                .device_id_for(&network)
                .unwrap()
        };
        let ids: Vec<_> = ["owner", "alice", "bob", "carol", "dave", "erin"]
            .iter()
            .map(|name| device(name))
            .collect();
        let member = |index: usize, by: Option<usize>, full: bool| {
            let mut member = template.clone();
            member.device_id = ids[index].clone();
            member.approved_by = by.map(|by| ids[by].clone());
            member.permissions = if full {
                tako_network::Permissions::Full
            } else {
                tako_network::Permissions::Deploy(["api/prod".parse().unwrap()].into())
            };
            member
        };
        // owner -> alice -> bob, alice -> carol (revoked) -> dave; erin is separate.
        let members = [
            member(0, None, true),
            member(1, Some(0), true),
            member(2, Some(1), false),
            member(3, Some(1), true),
            member(4, Some(3), false),
            member(5, None, true),
        ];
        let statuses = [Active, Active, Active, Revoked, Active, Active];
        let names = ["owner", "alice", "bob", "carol", "dave", "erin"];
        let rows: Vec<_> = members
            .iter()
            .zip(statuses)
            .zip(names)
            .map(|((member, status), name)| ClientRow {
                name: name.into(),
                member,
                status,
                invited_by: "-".into(),
            })
            .collect();
        let subtree: Vec<_> = invited_subtree(&rows, &ids[1])
            .into_iter()
            .map(|row| row.name.as_str())
            .collect();
        // Carol is already revoked, but the client she invited is still found.
        assert_eq!(subtree, ["bob", "dave"]);
        let with = |root: usize| {
            let mut targets = vec![&rows[root]];
            targets.extend(invited_subtree(&rows, &ids[root]));
            targets
        };
        // Erin keeps the network administrable, and owner runs the command.
        assert_eq!(refuse_revoking_together(&rows, &with(1), &ids[0]), None);
        let own = refuse_revoking_together(&rows, &with(1), &ids[2]).unwrap();
        assert!(own.contains("client bob"), "{own}");
        let mut everyone = with(0);
        everyone.push(&rows[5]);
        let last = refuse_revoking_together(&rows, &everyone, &device("outsider")).unwrap();
        assert!(last.contains("without a full client"), "{last}");
    }
}
