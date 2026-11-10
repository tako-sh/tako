ensure_privileged_bind_capability() {
  : # Native units grant capabilities to the proxy; the agent binary has none.
}

if is_enabled "$TAKO_RESTART_SERVICE" && [ "$SERVICE_MANAGER" = "none" ]; then
  echo "error: systemd must be running to start tako-server" >&2
  exit 1
fi

if [ -z "$TAKO_VPN" ] && [ -r /usr/local/libexec/tako/vpn-provider ]; then
  IFS= read -r TAKO_VPN < /usr/local/libexec/tako/vpn-provider
fi
TAKO_VPN="${TAKO_VPN:-native}"
if [ "$TAKO_VPN" != native ]; then
  echo "error: only native Tako networking is supported; re-enroll with tako servers add admin@host --network <network>" >&2
  exit 1
fi

maybe_prompt_ssh_pubkey() {
  is_valid_ssh_public_key() {
    key_line="$1"
    key_type="$(printf '%s\n' "$key_line" | awk '{print $1}')"
    key_blob="$(printf '%s\n' "$key_line" | awk '{print $2}')"

    if [ -z "$key_type" ] || [ -z "$key_blob" ]; then
      return 1
    fi

    case "$key_type" in
      ssh-ed25519|ssh-rsa|ssh-dss|ecdsa-sha2-nistp256|ecdsa-sha2-nistp384|ecdsa-sha2-nistp521|sk-ssh-ed25519@openssh.com|sk-ecdsa-sha2-nistp256@openssh.com)
        ;;
      *)
        return 1
        ;;
    esac

    printf '%s\n' "$key_blob" | grep -Eq '^[A-Za-z0-9+/=]+$'
  }

  first_valid_authorized_key() {
    auth_file="$1"
    if [ ! -r "$auth_file" ]; then
      return 1
    fi
    awk '
      /^[[:space:]]*#/ { next }
      NF < 2 { next }
      $1 ~ /^(ssh-ed25519|ssh-rsa|ssh-dss|ecdsa-sha2-nistp256|ecdsa-sha2-nistp384|ecdsa-sha2-nistp521|sk-ssh-ed25519@openssh.com|sk-ecdsa-sha2-nistp256@openssh.com)$/ && $2 ~ /^[A-Za-z0-9+\/=]+$/ { print $1 " " $2; exit }
    ' "$auth_file"
  }

  maybe_use_invoking_user_key() {
    invoking_user="${SUDO_USER:-}"
    if [ -z "$invoking_user" ] || [ "$invoking_user" = "root" ]; then
      return 1
    fi

    invoking_home=""
    if need_cmd getent; then
      invoking_home="$(getent passwd "$invoking_user" 2>/dev/null | awk -F: '{print $6}' || true)"
    fi
    if [ -z "$invoking_home" ]; then
      invoking_home="$(awk -F: -v u="$invoking_user" '$1==u {print $6}' /etc/passwd 2>/dev/null || true)"
    fi
    if [ -z "$invoking_home" ]; then
      return 1
    fi

    fallback_key="$(first_valid_authorized_key "$invoking_home/.ssh/authorized_keys" || true)"
    if ! is_valid_ssh_public_key "$fallback_key"; then
      return 1
    fi

    TAKO_SSH_PUBKEY="$fallback_key"
    echo "OK using SSH key from '$invoking_user' authorized_keys for '$TAKO_USER'"
    return 0
  }

  if [ -n "${TAKO_SSH_PUBKEY:-}" ]; then
    if ! is_valid_ssh_public_key "$TAKO_SSH_PUBKEY"; then
      echo "error: TAKO_SSH_PUBKEY must be a single SSH public key line (for example: ssh-ed25519 AAAA...)." >&2
      exit 1
    fi
    return
  fi

  echo "SSH setup:"
  echo "  To allow SSH login as '$TAKO_USER', paste your public key."
  echo "  Get one from your local machine with: cat ~/.ssh/id_ed25519.pub"
  echo "  If needed, create one with: ssh-keygen -t ed25519"

  if [ -t 0 ] && [ -t 1 ]; then
    while :; do
      printf "Public key for '$TAKO_USER': "
      if ! IFS= read -r TAKO_SSH_PUBKEY; then
        if ! maybe_use_invoking_user_key; then
          echo "warning: could not read SSH key input; skipping SSH key setup." >&2
          echo "warning: re-run with TAKO_SSH_PUBKEY='ssh-ed25519 ...' to install a key." >&2
          TAKO_SSH_PUBKEY=""
        fi
        break
      fi
      if is_valid_ssh_public_key "$TAKO_SSH_PUBKEY"; then
        break
      fi
      echo "warning: invalid SSH public key format. Paste the full key line (for example: ssh-ed25519 AAAA...)." >&2
    done
  elif [ -r /dev/tty ] && [ -w /dev/tty ] && (printf '' > /dev/tty) 2>/dev/null; then
    # Support common piped installs (curl ... | sudo sh) by prompting on the controlling tty.
    while :; do
      printf "Public key for '$TAKO_USER': " > /dev/tty
      if ! IFS= read -r TAKO_SSH_PUBKEY < /dev/tty; then
        if ! maybe_use_invoking_user_key; then
          echo "warning: could not read SSH key input from terminal; skipping SSH key setup." > /dev/tty
          echo "warning: re-run with TAKO_SSH_PUBKEY='ssh-ed25519 ...' to install a key." > /dev/tty
          TAKO_SSH_PUBKEY=""
        fi
        break
      fi
      if is_valid_ssh_public_key "$TAKO_SSH_PUBKEY"; then
        break
      fi
      echo "warning: invalid SSH public key format. Paste the full key line (for example: ssh-ed25519 AAAA...)." > /dev/tty
    done
  else
    if ! maybe_use_invoking_user_key; then
      echo "warning: non-interactive install; skipping SSH key prompt." >&2
      echo "warning: re-run with TAKO_SSH_PUBKEY='ssh-ed25519 ...' to install a key." >&2
    fi
  fi
}

