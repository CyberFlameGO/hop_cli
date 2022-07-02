use crate::state::State;
use crate::types::{Base, Projects};
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "hop project switch",
    about = "🚦 Switch to a different project"
)]
pub struct SwitchOptions {}

pub async fn handle_switch(mut state: State) -> Result<(), std::io::Error> {
    let response = state
        .http
        .client
        .get(format!("{}/users/@me", state.http.base_url))
        .send()
        .await
        .expect("Error while getting project info: {}");

    let user = response
        .json::<Base<Projects>>()
        .await
        .expect("Error while parsing json");

    let projects = user.data.projects;

    let projects_fmt = projects
        .iter()
        .map(|p| format!("{} ({})", p.name, p.namespace))
        .collect::<Vec<_>>();

    let idx = dialoguer::Select::new()
        .with_prompt("Select a project to set as default (use arrow keys and enter to select)")
        .items(&projects_fmt)
        .default(if let Some(id) = state.ctx.project {
            projects.iter().position(|p| p.id == id).unwrap_or(0)
        } else {
            0
        })
        .interact_opt()
        .unwrap();

    state.ctx.project = Some(projects[idx.expect("No project selected")].id.clone());
    state.ctx.save().await?;

    Ok(())
}
