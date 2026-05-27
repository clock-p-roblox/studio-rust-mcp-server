use color_eyre::eyre::{Result, WrapErr};
use roblox_install::RobloxStudio;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::{fs, io};

pub fn plugin_bytes() -> &'static [u8] {
    include_bytes!(concat!(env!("OUT_DIR"), "/MCPStudioPlugin.rbxm"))
}

pub fn write_plugin_to_path(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = File::create(path)
        .wrap_err_with(|| format!("Could not create plugin file at {}", path.display()))?;
    file.write_all(plugin_bytes())
        .wrap_err_with(|| format!("Could not write plugin file at {}", path.display()))?;
    Ok(())
}

fn get_message(successes: String) -> String {
    format!("clockp MCP Studio plugin is ready.
Please restart Studio to apply the plugin changes.

Tools included:
- run_code
- insert_model
- get_console_output
- launch_studio_session
- start_stop_play
- run_script_in_play_mode
- get_studio_mode

Plugin integration:
{successes}

Note: clock-p platform users should access Roblox Studio through clock-p skills and high-level scripts, not by configuring this server directly as an LLM MCP client.
To uninstall, delete the MCPStudioPlugin.rbxm from your Plugins directory.")
}

async fn install_internal() -> Result<String> {
    let studio = RobloxStudio::locate()?;
    let plugins = studio.plugins_path();
    if let Err(err) = fs::create_dir(plugins) {
        if err.kind() != io::ErrorKind::AlreadyExists {
            return Err(err.into());
        }
    }
    let output_plugin = Path::new(&plugins).join("MCPStudioPlugin.rbxm");
    write_plugin_to_path(&output_plugin)?;
    println!(
        "Installed Roblox Studio plugin to {}",
        output_plugin.display()
    );

    println!();
    let msg = get_message(
        "Managed by clock-p platform skills; no direct MCP client config written.".to_string(),
    );
    println!("{msg}");
    Ok(msg)
}

#[cfg(target_os = "windows")]
pub async fn install() -> Result<()> {
    use std::process::Command;
    if let Err(e) = install_internal().await {
        tracing::error!("Failed initialize clockp MCP Studio plugin: {:#}", e);
    }
    let _ = Command::new("cmd.exe").arg("/c").arg("pause").status();
    Ok(())
}

#[cfg(target_os = "macos")]
pub async fn install() -> Result<()> {
    use native_dialog::{DialogBuilder, MessageLevel};
    let alert_builder = match install_internal().await {
        Err(e) => DialogBuilder::message()
            .set_level(MessageLevel::Error)
            .set_text(format!("Errors occurred: {e:#}")),
        Ok(msg) => DialogBuilder::message()
            .set_level(MessageLevel::Info)
            .set_text(msg),
    };
    let _ = alert_builder
        .set_title("clockp MCP Studio plugin")
        .alert()
        .show();
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub async fn install() -> Result<()> {
    install_internal().await?;
    Ok(())
}
