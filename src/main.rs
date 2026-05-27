mod api_error;
mod automations;
mod conditions;
mod devices;
mod energy;
mod hooks;
mod legacy;
mod migration;
mod schedules;
mod settings;
mod state;
mod time;
mod web;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    legacy::run().await
}
