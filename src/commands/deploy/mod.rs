mod util;

use std::env::current_dir;
use std::path::PathBuf;

use hyper::Method;
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use serde_json::Value;
use structopt::StructOpt;
use tokio::fs;

use self::util::compress;
use super::ignite::types::RamSizes;
use crate::commands::deploy::util::create_deployment_config;
use crate::commands::ignite::types::{ContainerType, ScalingStrategy, SingleDeployment};
use crate::config::HOP_BUILD_BASE_URL;
use crate::state::State;
use crate::store::hopfile::HopFile;
use crate::{done, info, warn};

#[derive(Debug, StructOpt, Default, PartialEq)]
pub struct DeploymentConfig {
    #[structopt(
        short = "n",
        long = "name",
        help = "Name of the deployment, defaults to the directory name"
    )]
    name: Option<String>,

    #[structopt(
        short = "t",
        long = "type",
        help = "Type of the container, defaults to `ephemeral`"
    )]
    container_type: Option<ContainerType>,

    #[structopt(
        short = "c",
        long = "cpu",
        help = "The number of CPUs to use, defaults to 1"
    )]
    cpu: Option<u64>,

    #[structopt(
        short = "m",
        long = "ram",
        help = "Amount of RAM to use, defaults to 512MB"
    )]
    ram: Option<RamSizes>,

    #[structopt(
        short = "e",
        long = "env",
        help = "Environment variables to set, in the form of KEY=VALUE"
    )]
    env: Option<Vec<String>>,

    #[structopt(
        short = "s",
        long = "scaling",
        help = "Scaling strategy, defaults to `manual`"
    )]
    scaling_strategy: Option<ScalingStrategy>,

    #[structopt(
        short = "i",
        long = "containers",
        help = "Number of containers to use, defaults to 1 if `scaling` is manual",
        required_if("scaling", "manual")
    )]
    containers: Option<u64>,
}

#[derive(Debug, StructOpt)]
#[structopt(about = "Deploy a new container")]
pub struct DeployOptions {
    #[structopt(
        name = "dir",
        help = "Directory to deploy, defaults to current directory"
    )]
    path: Option<PathBuf>,

    #[structopt(flatten)]
    config: DeploymentConfig,
}

pub async fn handle_deploy(options: DeployOptions, state: State) -> Result<(), std::io::Error> {
    let mut dir = current_dir().expect("Could not get current directory");

    if let Some(path) = options.path {
        dir = dir
            .join(path)
            .canonicalize()
            .expect("Could not get canonical path");
    }

    let mut connection = state
        .ws
        .connect(state.ctx.me.clone().unwrap().leap_token.as_str())
        .await;

    info!("Attempting to deploy {}", dir.display());

    let (_hopfile, deployment) = match HopFile::find(dir.clone()).await {
        Some(hopfile) => {
            info!("Found hopfile: {}", hopfile.path.display());

            // TODO: possible update of deployment if it already exists?
            let deployment = state
                .http
                .request::<SingleDeployment>(
                    "GET",
                    format!("/ignite/deployments/{}", hopfile.config.deployment_id).as_str(),
                    None,
                )
                .await
                .expect("Failed to get deployment")
                .unwrap()
                .deployment;

            if options.config != DeploymentConfig::default() {
                warn!("Deployment exists, skipping arguments");
            }

            (hopfile, deployment)
        }

        None => {
            info!("No hopfile found, creating one");

            let project = state.ctx.current_project_error();

            let mut hopfile = HopFile::new(
                dir.clone().join("hop.yml"),
                project.clone().id,
                // override later when created in the API
                String::new(),
            );

            // TODO: run a walkthrough to setup the deployment?
            let name = options
                .config
                .name
                .clone()
                .unwrap_or_else(|| dir.file_name().unwrap().to_str().unwrap().to_string());

            let deployment_config =
                create_deployment_config(options.config, name, project.namespace);

            let deployment = state
                .http
                .request::<SingleDeployment>(
                    "POST",
                    format!(
                        "/ignite/deployments?project={}",
                        hopfile.config.project_id.clone()
                    )
                    .as_str(),
                    Some((
                        serde_json::to_string(&deployment_config).unwrap().into(),
                        "application/json",
                    )),
                )
                .await
                .expect("Error while creating deployment")
                .unwrap()
                .deployment;

            hopfile.config.deployment_id = deployment.id.clone();

            hopfile
                .clone()
                .save()
                .await
                .expect("Could not save hopfile");

            (hopfile, deployment)
        }
    };

    // deployment id is used not to colide if the user is deploying multiple items
    let packed = compress(deployment.id.clone(), dir)
        .await
        .expect("Could not compress");

    info!("Packed to: {}", packed);

    let bytes = fs::read(packed.clone())
        .await
        .expect("Could not read packed file");
    let multipart = Form::new().part(
        "file",
        Part::bytes(bytes)
            .file_name("deployment.tar.gz")
            .mime_str("application/x-gzip")
            .unwrap(),
    );

    info!("Uploading...");

    let response = state
        .http
        .client
        .request(
            Method::POST,
            format!(
                "{}/deployments/{}/builds",
                HOP_BUILD_BASE_URL, deployment.id
            )
            .as_str(),
        )
        .header("content_type", "multipart/form-data".to_string())
        .multipart(multipart)
        .send()
        .await
        .expect("Failed to send data to build endpoint");

    state
        .http
        .handle_response::<()>(response)
        .await
        .expect("Failed to handle response");

    info!("Deleting archive...");
    fs::remove_file(packed).await?;

    #[derive(Debug, Deserialize)]
    struct Data {
        d: Option<String>,
        e: String,
    }

    #[derive(Debug, Deserialize)]
    struct Message {
        d: Value,
        e: String,
    }

    info!("From Hop builder:");

    while let Some(data) = connection.recieve_message::<Message>().await {
        // build logs are sent only in DMs
        if data.e != "DIRECT_MESSAGE" {
            continue;
        }

        let data: Data = serde_json::from_value(data.d).unwrap();

        if let Some(data) = data.d {
            print!("{}", data);
        }

        match data.e.as_str() {
            "PUSH_SUCCESS" => {
                connection.close().await;
                println!("");
                info!("Pushed successfully");
                break;
            }

            "PUSH_FAILURE" => {
                connection.close().await;
                println!("");
                panic!("Push failed, for help contact us on https://discord.gg/hop and mention the deployment id: {}", deployment.id);
            }

            // ignore rest
            _ => {}
        }
    }

    done!("Pushed deployment `{}`", deployment.name);

    // TODO: ask to deploy containers

    Ok(())
}
