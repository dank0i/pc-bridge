//! Custom command execution - user-defined commands from config
#![allow(dead_code)] // Platform-specific execution

use log::{debug, error, info};
use std::sync::Arc;

use crate::AppState;
use crate::config::{CustomCommand, CustomCommandType};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use std::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Execute a custom command by name
/// Returns Ok(true) if command was found and executed, Ok(false) if not found
pub async fn execute_custom_command(state: &Arc<AppState>, name: &str) -> anyhow::Result<bool> {
    let config = state.config.read().await;

    // Check if custom commands are enabled
    if !config.custom_commands_enabled {
        debug!("Custom commands disabled, ignoring: {}", name);
        return Ok(false);
    }

    // Find the command
    let cmd = match config.custom_commands.iter().find(|c| c.name == name) {
        Some(c) => c.clone(),
        None => return Ok(false),
    };

    // Check admin permission
    if cmd.admin && !config.custom_command_privileges_allowed {
        error!(
            "Custom command '{}' requires admin but custom_command_privileges_allowed=false",
            name
        );
        return Err(anyhow::anyhow!(
            "Admin command blocked - custom_command_privileges_allowed is false"
        ));
    }

    drop(config); // Release lock before executing

    info!("Executing custom command: {} (admin={})", name, cmd.admin);

    // Execute based on type
    match cmd.command_type {
        CustomCommandType::Powershell => execute_powershell(&cmd).await,
        CustomCommandType::Executable => execute_executable(&cmd).await,
        CustomCommandType::Shell => execute_shell(&cmd).await,
    }?;

    Ok(true)
}

/// Execute PowerShell command
#[cfg(windows)]
async fn execute_powershell(cmd: &CustomCommand) -> anyhow::Result<()> {
    let script = cmd
        .script
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No script for powershell command"))?
        .clone();
    let admin = cmd.admin;

    tokio::task::spawn_blocking(move || {
        if admin {
            // Run elevated via Start-Process -Verb RunAs
            let escaped = script.replace('\'', "''");
            let ps_cmd = format!(
                "Start-Process powershell -Verb RunAs -ArgumentList '-NoProfile -Command \"{}\"'",
                escaped
            );

            Command::new("powershell")
                .args(["-NoProfile", "-Command", &ps_cmd])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()?;
        } else {
            Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()?;
        }

        Ok::<_, anyhow::Error>(())
    })
    .await??;

    Ok(())
}

#[cfg(unix)]
async fn execute_powershell(_cmd: &CustomCommand) -> anyhow::Result<()> {
    Err(anyhow::anyhow!("PowerShell not available on this platform"))
}

/// Execute an executable file
#[cfg(windows)]
async fn execute_executable(cmd: &CustomCommand) -> anyhow::Result<()> {
    let path = cmd
        .path
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No path for executable command"))?
        .clone();
    let args = cmd.args.clone().unwrap_or_default();
    let admin = cmd.admin;

    tokio::task::spawn_blocking(move || {
        if admin {
            // Run elevated via Start-Process -Verb RunAs
            let args_str = args.join(" ");
            let ps_cmd = format!(
                "Start-Process '{}' -Verb RunAs -ArgumentList '{}'",
                path, args_str
            );

            Command::new("powershell")
                .args(["-NoProfile", "-Command", &ps_cmd])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()?;
        } else {
            Command::new(&path)
                .args(&args)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()?;
        }

        Ok::<_, anyhow::Error>(())
    })
    .await??;

    Ok(())
}

#[cfg(unix)]
async fn execute_executable(cmd: &CustomCommand) -> anyhow::Result<()> {
    use std::process::Command;

    let path = cmd
        .path
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No path for executable command"))?
        .clone();
    let args = cmd.args.clone().unwrap_or_default();
    let admin = cmd.admin;

    tokio::task::spawn_blocking(move || {
        if admin {
            Command::new("sudo").arg(&path).args(&args).spawn()?;
        } else {
            Command::new(&path).args(&args).spawn()?;
        }

        Ok::<_, anyhow::Error>(())
    })
    .await??;

    Ok(())
}

/// Execute a shell command
#[cfg(windows)]
async fn execute_shell(cmd: &CustomCommand) -> anyhow::Result<()> {
    let command = cmd
        .command
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No command for shell command"))?
        .clone();
    let admin = cmd.admin;

    tokio::task::spawn_blocking(move || {
        if admin {
            let escaped = command.replace('\'', "''");
            let ps_cmd = format!(
                "Start-Process cmd -Verb RunAs -ArgumentList '/c {}'",
                escaped
            );

            Command::new("powershell")
                .args(["-NoProfile", "-Command", &ps_cmd])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()?;
        } else {
            Command::new("cmd")
                .args(["/c", &command])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()?;
        }

        Ok::<_, anyhow::Error>(())
    })
    .await??;

    Ok(())
}

#[cfg(unix)]
async fn execute_shell(cmd: &CustomCommand) -> anyhow::Result<()> {
    use std::process::Command;

    let command = cmd
        .command
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No command for shell command"))?
        .clone();
    let admin = cmd.admin;

    tokio::task::spawn_blocking(move || {
        if admin {
            Command::new("sudo").args(["sh", "-c", &command]).spawn()?;
        } else {
            Command::new("sh").args(["-c", &command]).spawn()?;
        }

        Ok::<_, anyhow::Error>(())
    })
    .await??;

    Ok(())
}
