use crate::control::State;
use std::sync::{Arc, Mutex};

pub fn install(state: &Arc<Mutex<State>>, workflows: &tako_workflows::WorkflowManager) {
    let state = Arc::downgrade(state);
    workflows.set_network_router(Arc::new(move |_caller, target, invocation| {
        let Some(state) = state.upgrade() else {
            return tako_core::Response::coded_error(
                tako_core::ErrorCode::Unavailable,
                "dev server unavailable",
            );
        };
        let selected = {
            let state = state.lock().unwrap();
            let mut matches = state.apps.iter().filter(|(_, app)| app.name == target);
            let first = matches
                .next()
                .map(|(key, app)| (key.clone(), app.clone(), state.routes.clone()));
            if matches.next().is_some() {
                return tako_core::Response::error("ambiguous development app namespace");
            }
            first
        };
        let Some((key, app, routes)) = selected else {
            return tako_core::Response::error("unknown development app");
        };
        tokio::runtime::Handle::current().block_on(async move {
            if app.is_idle {
                crate::process::wake_registration(state.clone(), &key, None).await;
            }
            let Some(port) = routes
                .wait_for_active_port(&format!("reg:{key}"), std::time::Duration::from_secs(20))
                .await
            else {
                return tako_core::Response::coded_error(
                    tako_core::ErrorCode::Unavailable,
                    "destination app unavailable",
                );
            };
            tako_workflows::network::invoke_http(
                &format!("http://127.0.0.1:{port}/network"),
                &format!("{}.tako", app.name),
                Some(&app.bootstrap_token),
                &invocation,
            )
            .await
        })
    }));
}
