use super::super::request::{
    create_production_error_response, insert_body_headers, static_lookup_paths, stream_static_file,
};
use super::super::{AppStaticServer, StaticConfig, StaticFileError, TakoProxy};
use super::RequestCtx;
use pingora_core::prelude::*;
use pingora_http::ResponseHeader;
use pingora_proxy::Session;
use std::path::Path;
use std::sync::Arc;

impl TakoProxy {
    pub(crate) fn static_server_for_app(
        &self,
        app_name: &str,
        app_root: &Path,
    ) -> Arc<AppStaticServer> {
        let desired_root = app_root.join(&default_static_config().public_dir);
        if let Some(existing) = self.static_servers.read().get(app_name)
            && existing.root() == desired_root.as_path()
        {
            return existing.clone();
        }

        let mut servers = self.static_servers.write();
        if let Some(existing) = servers.get(app_name)
            && existing.root() == desired_root.as_path()
        {
            return existing.clone();
        }

        let server = Arc::new(AppStaticServer::new(
            app_root.to_path_buf(),
            default_static_config().clone(),
        ));
        servers.insert(app_name.to_string(), server.clone());
        server
    }

    pub(crate) async fn try_serve_static_asset(
        &self,
        session: &mut Session,
        ctx: &mut RequestCtx,
        app_name: &str,
        request_path: &str,
        matched_route_path: Option<&str>,
    ) -> Result<bool> {
        let method = session.req_header().method.as_str();
        let is_head = method == "HEAD";
        if method != "GET" && !is_head {
            ctx.observation.set_handler("static", "bypass");
            return Ok(false);
        }

        let Some(app) = self.lb.app_manager().get_app(app_name) else {
            ctx.observation.set_handler("static", "bypass");
            return Ok(false);
        };
        let app_root = app.config.read().path.clone();
        let static_server = self.static_server_for_app(app_name, &app_root);
        if !static_server.is_available() {
            ctx.observation.set_handler("static", "bypass");
            return Ok(false);
        }

        for lookup_path in static_lookup_paths(request_path, matched_route_path) {
            match static_server.resolve(&lookup_path) {
                Ok(file) => {
                    ctx.observation.set_handler("static", "hit");
                    let mut file_handle = if is_head {
                        None
                    } else {
                        match tokio::fs::File::open(&file.path).await {
                            Ok(opened) => Some(opened),
                            Err(error) => {
                                ctx.observation.set_handler("static", "error");
                                tracing::error!(
                                    app = %app_name,
                                    path = %file.path.display(),
                                    "Static asset read failed: {error}"
                                );
                                return create_production_error_response(session, 500).await;
                            }
                        }
                    };

                    let mut header = ResponseHeader::build(200, None)?;
                    header.insert_header("Content-Type", &file.content_type)?;
                    header.insert_header("Content-Length", file.size.to_string())?;
                    header.insert_header("Cache-Control", &file.cache_control)?;
                    header.insert_header("ETag", &file.etag)?;
                    session
                        .write_response_header(Box::new(header), false)
                        .await?;

                    if is_head {
                        session.write_response_body(None, true).await?;
                        return Ok(true);
                    }

                    let mut file_handle = file_handle
                        .take()
                        .expect("file handle is always present for non-HEAD static responses");
                    stream_static_file(session, &mut file_handle, &file.path).await?;
                    return Ok(true);
                }
                Err(StaticFileError::NotFound(_)) => {}
                Err(StaticFileError::PathTraversal(_)) | Err(StaticFileError::InvalidPath(_)) => {
                    ctx.observation.set_handler("static", "error");
                    let body = "Bad Request";
                    let mut header = ResponseHeader::build(400, None)?;
                    insert_body_headers(&mut header, "text/plain", body)?;
                    session
                        .write_response_header(Box::new(header), false)
                        .await?;
                    session.write_response_body(Some(body.into()), true).await?;
                    return Ok(true);
                }
                Err(StaticFileError::Io(_)) => {}
            }
        }

        ctx.observation.set_handler("static", "miss");
        Ok(false)
    }
}

fn default_static_config() -> &'static StaticConfig {
    static CONFIG: std::sync::OnceLock<StaticConfig> = std::sync::OnceLock::new();
    CONFIG.get_or_init(StaticConfig::default)
}
