use rust_ios_protocol::{CommandInvocation, CommandResult};

use crate::{CoreResult, app_store_connect, signing, ui};

/// 执行不依赖设备选择的 host 命令；server 在调用 backend 前先经过这里。
pub async fn execute_host_command(
    invocation: &CommandInvocation,
) -> CoreResult<Option<CommandResult>> {
    let result = match invocation.argv.as_slice() {
        [command, _, service, ..] if command == "sign" && service == "appstoreconnect" => {
            Some(app_store_connect::execute(invocation).await?)
        }
        [command, action, ..] if command == "ui" && action == "download" => {
            Some(ui::download(invocation).await?)
        }
        [command, action, ..]
            if command == "sign"
                && action == "app"
                && !invocation.argv.iter().any(|value| value == "--install") =>
        {
            Some(signing::sign_app_command(invocation).await?)
        }
        _ => None,
    };
    Ok(result)
}
