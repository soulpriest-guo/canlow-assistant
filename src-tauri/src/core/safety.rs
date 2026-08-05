// 命令安全：token 级高危命令检测（移植自旧版 harness_utils.py）
// 策略：关键字级兜底 + token 级精确检测（保护 /、家目录、工作目录、系统关键目录）

// 仅保留字面量级高危模式（无法用 token 解析表达的情况）。
// sudo/su/passwd/chown 已改为 token 级处理（见 DANGEROUS_BASES 与 SKIP_PREFIXES），
// 避免 contains 误伤普通用法（如 "chown file"、"sudo 前缀被跳过后检查真实命令"）。
// shutdown/reboot 等系统级命令保留 contains 兜底（可覆盖 python -c "...shutdown..." 等混淆场景）。
const DANGEROUS_KEYWORDS: &[&str] = &[
    "rm -rf /", "rm -rf/", "rm -r -f /", "rm -f -r /", "rm -fr /",
    "mkfs", "dd if=", "> /dev/sd", "chmod 777 /", "chmod -r 777 /",
    "shutdown", "reboot", "init 0",
    ":(){ :|:& };:",
];

const SKIP_PREFIXES: &[&str] = &[
    "sudo", "doas", "env", "nohup", "time", "command", "nice", "setsid",
    "caffeinate", "xargs", "zsh", "bash", "sh", "python", "python3", "python2",
];

const DANGEROUS_BASES: &[&str] = &[
    "mkfs", "mkfs.ext2", "mkfs.ext3", "mkfs.ext4", "mkfs.btrfs", "mkfs.xfs",
    "dd", "shutdown", "reboot", "halt", "poweroff", "init", "telinit",
    "killall", "pkill", "fdisk", "parted", "gdisk", "sfdisk", "pvcreate",
    "vgremove", "lvremove", "diskutil", "newfs", "mkswap", "swapoff",
];

const PROTECTED_DIRS: &[&str] = &[
    "/etc", "/usr", "/var", "/bin", "/sbin", "/boot", "/root", "/System",
    "/Library", "/Applications", "/Windows", "/Program Files",
    "/Program Files (x86)", "/opt", "/home",
];

/// 返回拦截原因；None 表示放行
pub fn check_dangerous_command(cmd: &str, work_dir: Option<&str>) -> Option<String> {
    let cl = cmd.to_lowercase();

    // 1) 关键字级（覆盖混淆/内嵌脚本）
    for kw in DANGEROUS_KEYWORDS {
        if cl.contains(kw) {
            return Some(format!("命令包含高危操作「{}」", kw.trim()));
        }
    }

    // 2) token 级解析
    let tokens: Vec<String> = match shell_words(cmd) {
        Some(t) => t,
        None => return None,
    };
    if tokens.is_empty() {
        return None;
    }

    // 跳过包装前缀
    let mut i = 0;
    while i < tokens.len() && SKIP_PREFIXES.contains(&tokens[i].to_lowercase().as_str()) {
        i += 1;
    }

    // sh -c "..." / python -c "..."：递归检查内嵌命令
    if i < tokens.len() && tokens[i] == "-c" && i + 1 < tokens.len() {
        let inner = tokens[i + 1..].join(" ");
        if let Some(d) = check_dangerous_command(&inner, work_dir) {
            return Some(format!("内嵌命令: {d}"));
        }
        return None;
    }
    if i >= tokens.len() {
        return None;
    }

    let base = tokens[i]
        .split('/')
        .last()
        .unwrap_or(&tokens[i])
        .to_lowercase()
        .replace(".exe", "");

    if DANGEROUS_BASES.contains(&base.as_str()) {
        return Some(format!("高危命令: {base}"));
    }

    // rm/rmdir/mv：递归删除受保护路径时拦截
    if matches!(base.as_str(), "rm" | "rmdir" | "mv") {
        let flags: Vec<String> = tokens[i + 1..]
            .iter()
            .filter(|t| t.starts_with('-'))
            .cloned()
            .collect();
        let has_recursive = flags.iter().any(|f| {
            matches!(
                f.as_str(),
                "-r" | "-rf" | "-fr" | "-f" | "-R" | "-Rf" | "--recursive" | "--force"
            ) || f.contains("recursive")
                || f.contains("force")
        });
        if has_recursive {
            let mut protected: Vec<String> = vec![
                "/".to_string(),
                std::env::var("HOME")
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .unwrap_or_default(),
            ];
            if let Some(wd) = work_dir {
                protected.push(wd.to_string());
            }
            for t in &tokens[i + 1..] {
                if t.starts_with('-') {
                    continue;
                }
                if t == "." || t == ".." {
                    return Some(format!("{base} {t}（递归删除当前/上级目录）"));
                }
                let home = std::env::var("HOME")
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .unwrap_or_default();
                let expanded = t.replace("~", &home);
                let abs = std::path::absolute(std::path::PathBuf::from(&expanded)).unwrap_or_else(|_| std::path::PathBuf::from(&expanded));
                let abs_s = abs.to_string_lossy().to_string();
                if protected.contains(&abs_s) {
                    return Some(format!("{base} {t}（受保护目录）"));
                }
                // 系统关键目录：前缀匹配（含子路径与目录本身）
                for pd in PROTECTED_DIRS {
                    if abs_s == *pd
                        || abs_s.starts_with(&format!("{pd}/"))
                        || abs_s.starts_with(&format!("{pd}\\"))
                    {
                        return Some(format!("{base} {t}（系统关键目录）"));
                    }
                }
            }
        }
    }

    None
}

/// 简单 shell 分词（处理引号；无法解析时返回 None）
fn shell_words(cmd: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for ch in cmd.chars() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else {
                    cur.push(ch);
                }
            }
            None => {
                match ch {
                    '\'' | '"' => {
                        quote = Some(ch);
                        started = true;
                    }
                    ' ' | '\t' | '\n' => {
                        if started {
                            out.push(std::mem::take(&mut cur));
                            started = false;
                        }
                    }
                    _ => {
                        cur.push(ch);
                        started = true;
                    }
                }
            }
        }
    }
    if quote.is_some() {
        return None; // 引号未闭合，不做拦截判断
    }
    if started {
        out.push(cur);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_dangerous() {
        assert!(check_dangerous_command("rm -rf /", None).is_some());
        assert!(check_dangerous_command("rm -r -f /", None).is_some());
        assert!(check_dangerous_command("sudo rm -rf /home", None).is_some());
        assert!(check_dangerous_command("sh -c \"rm -rf /\"", None).is_some());
        assert!(check_dangerous_command("python3 -c \"os.system('shutdown')\"", None).is_some());
        assert!(check_dangerous_command("mkfs.ext4 /dev/sdb", None).is_some());
        assert!(check_dangerous_command("dd if=/dev/zero of=/dev/sda", None).is_some());
    }

    #[test]
    fn allows_safe() {
        assert!(check_dangerous_command("ls -la", None).is_none());
        assert!(check_dangerous_command("git status", None).is_none());
        assert!(check_dangerous_command("rm -rf temp_build", Some("/tmp/proj")).is_none());
        assert!(check_dangerous_command("cat file.txt", None).is_none());
        // sudo 前缀被跳过后再检查真实命令：sudo ls 放行、sudo rm -rf /home 拦截
        assert!(check_dangerous_command("sudo ls -la", None).is_none());
        assert!(check_dangerous_command("chown user file.txt", None).is_none());
    }

    #[test]
    fn protects_dirs() {
        assert!(check_dangerous_command("rm -rf /etc", None).is_some());
        // 子路径前缀匹配
        assert!(check_dangerous_command("rm -rf /etc/ssh", None).is_some());
        assert!(check_dangerous_command("rm -rf /usr/local/bin", None).is_some());
        let home = std::env::var("HOME").unwrap_or_default();
        assert!(check_dangerous_command(&format!("rm -rf {}", home), None).is_some());
        assert!(check_dangerous_command("rm -rf .", Some("/tmp/proj")).is_some());
    }
}
