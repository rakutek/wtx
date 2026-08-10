use crate::sshx::vm_script;
use anyhow::{anyhow, Result};

/// ホストの Claude Code 資格情報をVMへ**コピー**する（マウントではない）。
/// ~/.claude を rw マウントすると VM 内エージェントがホスト側で実行される settings.json の
/// hooks を書き換えられ、隔離が破れるため。
pub fn copy_claude_creds(vm: &str) -> Result<()> {
    let src = dirs::home_dir()
        .unwrap_or_default()
        .join(".claude/.credentials.json");
    let data = std::fs::read(&src).map_err(|_| anyhow!("host credentials not found ({})", src.display()))?;
    vm_script(
        vm,
        "mkdir -p ~/.claude && cat > ~/.claude/.credentials.json && chmod 600 ~/.claude/.credentials.json",
        Some(&data),
    )
}
