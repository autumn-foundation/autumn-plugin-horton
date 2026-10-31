//! Two visit counters that survive a restart: one for the app, one for
//! each session.
//!
//! ```sh
//! cargo run --example visits
//! curl localhost:3000/visits
//! curl -c jar -b jar localhost:3000/me
//! ```

use autumn_plugin_horton::{Horton, HortonError, HortonPlugin};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::extract::State;
use autumn_web::session::Session;

#[get("/visits")]
async fn visits(State(state): State<AppState>) -> AutumnResult<String> {
    // Read, then write: two requests at once can count one visit. Use a
    // single writer or a `Batch` when the count must be exact.
    let Some(db) = state.extension::<Horton>() else {
        return Err(AutumnError::service_unavailable_msg("horton is not open"));
    };
    let count = db
        .get(b"visits")
        .await
        .map_err(HortonError::into_autumn)?
        .and_then(|v| <[u8; 8]>::try_from(v).ok())
        .map_or(0, u64::from_le_bytes)
        + 1;
    db.put(b"visits", &count.to_le_bytes())
        .await
        .map_err(HortonError::into_autumn)?;
    Ok(count.to_string())
}

/// Visits in this session. The session lives in the horton store.
#[get("/me")]
async fn me(session: Session) -> String {
    let count = session
        .get("visits")
        .await
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        + 1;
    session.insert("visits", count.to_string()).await;
    count.to_string()
}

#[autumn_web::main]
async fn main() {
    Box::pin(
        autumn_web::app()
            .plugin(
                HortonPlugin::new()
                    .path("target/visits.horton")
                    .sessions(true),
            )
            .routes(routes![visits, me])
            .run(),
    )
    .await;
}
