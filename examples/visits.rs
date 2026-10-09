//! A visit counter that survives a restart.
//!
//! ```sh
//! cargo run --example visits
//! curl localhost:3000/visits
//! ```

use autumn_plugin_horton::{Horton, HortonPlugin};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::extract::State;

#[get("/visits")]
async fn visits(State(state): State<AppState>) -> AutumnResult<String> {
    let Some(db) = state.extension::<Horton>() else {
        return Err(AutumnError::service_unavailable_msg("horton is not open"));
    };
    let count = db
        .get(b"visits")
        .await
        .map_err(AutumnError::internal_server_error)?
        .and_then(|v| <[u8; 8]>::try_from(v).ok())
        .map_or(0, u64::from_le_bytes)
        + 1;
    db.put(b"visits", &count.to_le_bytes())
        .await
        .map_err(AutumnError::internal_server_error)?;
    Ok(count.to_string())
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .plugin(HortonPlugin::new().path("target/visits.horton"))
        .routes(routes![visits])
        .run()
        .await;
}
