use anyhow::{Context, Result, bail};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

// ─────────────────────────────────────────────
// 配置结构
// ─────────────────────────────────────────────

/// 配置文件中未指定（留空）时使用的默认输出目录名（相对工作目录）
const DEFAULT_OUTPUT_DIRNAME: &str = "out";

#[derive(Debug, Serialize, Deserialize)]
struct Config {
    /// 视频放大默认存储目录，留空则使用工作目录下的 out 目录
    output_dir: String,
    /// ffmpeg 可执行文件路径
    ffmpeg_path: String,
    /// ffprobe 可执行文件路径
    ffprobe_path: String,
    /// realesrgan-ncnn-vulkan 可执行文件路径
    realesrgan_path: String,
    /// Real-ESRGAN 使用的模型名（-n 参数）
    /// 可选: realesr-animevideov3 | realesrgan-x4plus | realesrgan-x4plus-anime | realesrnet-x4plus
    realesrgan_model: String,
    /// 放大倍数（-s 参数），可选: 2 | 3 | 4
    realesrgan_scale: u32,
}

impl Config {
    /// 默认配置
    /// 所有路径一律写成【相对路径】，运行时以程序所在目录为基准解析，
    /// 这样整个目录被搬到任何位置都无需修改配置。
    fn default_for(_exe_dir: &Path) -> Self {
        Config {
            // 相对路径，基于程序所在目录；留空则回退到 out/
            output_dir: DEFAULT_OUTPUT_DIRNAME.to_string(),
            ffmpeg_path: "ffmpeg".to_string(),
            ffprobe_path: "ffprobe".to_string(),
            realesrgan_path: "realesrgan-ncnn-vulkan".to_string(),
            realesrgan_model: "realesr-animevideov3".to_string(),
            realesrgan_scale: 2,
        }
    }

    /// 生成带内联注释的 YAML 配置文件内容
    fn to_commented_yaml(&self) -> String {
        format!(
            r#"# 视频 AI 放大工具 配置文件
# ─────────────────────────────────────────────
# 工作目录：本程序所在目录（配置文件、临时目录均在此）
# 路径规则：所有路径都支持相对路径，且一律以【程序所在目录】为基准解析。
#           若指定路径不存在，会自动回退查找：
#             程序目录 → 程序目录/bin → 系统 PATH

# 视频放大默认输出目录（相对路径基于工作目录；留空则默认使用 工作目录/out 目录）
output_dir: "{}"

# ffmpeg 可执行文件（可只写文件名，如 ffmpeg 或 bin/ffmpeg）
ffmpeg_path: "{}"

# ffprobe 可执行文件（通常与 ffmpeg 同目录）
ffprobe_path: "{}"

# realesrgan-ncnn-vulkan 可执行文件
realesrgan_path: "{}"

# 使用的模型名（-n 参数）
# 可选: realesr-animevideov3 | realesrgan-x4plus | realesrgan-x4plus-anime | realesrnet-x4plus
realesrgan_model: "{}"

# 放大倍数（-s 参数），可选: 2 | 3 | 4
realesrgan_scale: {}
"#,
            self.output_dir,
            self.ffmpeg_path,
            self.ffprobe_path,
            self.realesrgan_path,
            self.realesrgan_model,
            self.realesrgan_scale,
        )
    }
}

// ─────────────────────────────────────────────
// 配置文件读写
// ─────────────────────────────────────────────

const CONFIG_FILENAME: &str = "config.yaml";



fn load_or_create_config(exe_dir: &Path) -> Result<Config> {
    let config_path = exe_dir.join(CONFIG_FILENAME);

    if !config_path.exists() {
        // 首次运行：生成默认配置
        let default_cfg = Config::default_for(exe_dir);
        let content = default_cfg.to_commented_yaml();
        fs::write(&config_path, &content)
            .with_context(|| format!("无法写入配置文件: {}", config_path.display()))?;
        println!(
            "{} 已生成默认配置文件: {}",
            "✔".green().bold(),
            config_path.display().to_string().cyan()
        );
    }

    let content = fs::read_to_string(&config_path)
        .with_context(|| format!("无法读取配置文件: {}", config_path.display()))?;
    let cfg: Config = serde_yaml::from_str(&content)
        .with_context(|| format!("配置文件格式错误: {}", config_path.display()))?;
    Ok(cfg)
}

// ─────────────────────────────────────────────
// 辅助：读取用户输入（去除首尾空白与引号）
// ─────────────────────────────────────────────

fn prompt(message: &str) -> Result<String> {
    print!("{}", message);
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    // 去除首尾空白与引号（Windows 拖拽文件会带引号）
    let trimmed = line.trim().trim_matches('"').trim_matches('\'').trim();
    Ok(trimmed.to_string())
}

// ─────────────────────────────────────────────
// 辅助：路径解析（一切以程序所在目录为基准）
// ─────────────────────────────────────────────

/// 把任意路径转成绝对路径：
/// - 绝对路径：原样返回
/// - 相对路径：以程序所在目录（base）为基准拼接
fn resolve_path(base: &Path, path: &str) -> PathBuf {
    let p = PathBuf::from(path.trim().trim_matches('"').trim_matches('\'').trim());
    if p.is_absolute() {
        p
    } else {
        base.join(p)
    }
}

/// 在系统 PATH 中查找可执行文件（只做文件查找，不实际运行，避免副作用）
fn find_in_path(exe_name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(exe_name))
        .find(|cand| cand.is_file())
}

/// 解析外部工具（ffmpeg / ffprobe / realesrgan）的可执行文件路径。
/// 查找顺序：
///   1. 配置中指定的路径（相对路径基于程序目录解析；若指定的是目录则在其下找exe）
///   2. 程序目录/<exe_name>
///   3. 程序目录/bin/<exe_name>
///   4. 系统 PATH
/// 都找不到时返回第 1 项（便于报错时展示用户配置的路径）。
fn resolve_tool(exe_dir: &Path, configured: &str, tool_name: &str) -> String {
    #[cfg(target_os = "windows")]
    let exe_name = format!("{}.exe", tool_name);
    #[cfg(not(target_os = "windows"))]
    let exe_name = tool_name.to_string();

    let mut candidates: Vec<PathBuf> = Vec::new();

    if !configured.trim().is_empty() {
        let configured_path = resolve_path(exe_dir, configured);
        candidates.push(configured_path.clone());
        // 配置项可能只写到"所在目录"
        candidates.push(configured_path.join(&exe_name));
    }
    candidates.push(exe_dir.join(&exe_name));
    candidates.push(exe_dir.join("bin").join(&exe_name));

    for cand in &candidates {
        if cand.is_file() {
            return cand.to_string_lossy().into_owned();
        }
    }

    if let Some(found) = find_in_path(&exe_name) {
        return found.to_string_lossy().into_owned();
    }

    candidates
        .first()
        .cloned()
        .unwrap_or_else(|| exe_dir.join(&exe_name))
        .to_string_lossy()
        .into_owned()
}

/// 检查可执行文件是否可用（已解析出的路径）
fn exe_exists(path: &str) -> bool {
    if Path::new(path).is_file() {
        return true;
    }
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(find_in_path)
        .is_some()
}

// ─────────────────────────────────────────────
// 辅助：获取 ffmpeg 版本字符串
// ─────────────────────────────────────────────

fn ffmpeg_version(ffmpeg_path: &str) -> Option<String> {
    let output = Command::new(ffmpeg_path)
        .arg("-version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    // 取第一行，如 "ffmpeg version 6.1.1 ..."
    text.lines().next().map(|l| l.to_string())
}

// ─────────────────────────────────────────────
// 辅助：统计目录中的文件数量
// ─────────────────────────────────────────────

fn count_files_in_dir(dir: &Path) -> usize {
    if !dir.exists() {
        return 0;
    }
    fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).filter(|e| e.path().is_file()).count())
        .unwrap_or(0)
}

// ─────────────────────────────────────────────
// 辅助：清理临时目录（失败不 panic）
// ─────────────────────────────────────────────

fn cleanup(dirs: &[&Path]) {
    for dir in dirs {
        if dir.exists() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

// ─────────────────────────────────────────────
// 主流程
// ─────────────────────────────────────────────

fn run() -> Result<()> {
    // 确定工作目录：程序所在目录（所有相对路径、配置文件、临时目录都以它为基准）
    let exe_path = std::env::current_exe().context("无法获取程序路径")?;
    let exe_dir = exe_path
        .parent()
        .context("无法获取程序所在目录")?
        .to_path_buf();

    // 切换工作目录
    std::env::set_current_dir(&exe_dir)
        .with_context(|| format!("无法切换到工作目录: {}", exe_dir.display()))?;

    // ── 读取/生成配置 ──────────────────────────
    let cfg = load_or_create_config(&exe_dir)?;

    // 以程序所在目录为基准解析外部工具路径
    let ffmpeg = resolve_tool(&exe_dir, &cfg.ffmpeg_path, "ffmpeg");
    let ffprobe = resolve_tool(&exe_dir, &cfg.ffprobe_path, "ffprobe");
    let realesrgan = resolve_tool(&exe_dir, &cfg.realesrgan_path, "realesrgan-ncnn-vulkan");

    // 解析输出目录（配置留空则使用工作目录下的 out；相对路径基于工作目录）
    let default_output_dir = if cfg.output_dir.trim().is_empty() {
        resolve_path(&exe_dir, DEFAULT_OUTPUT_DIRNAME)
    } else {
        resolve_path(&exe_dir, &cfg.output_dir)
    };

    // ── 欢迎信息 ──────────────────────────────
    println!();
    println!("{}", "═══════════════════════════════════════════════════════════".cyan());
    println!(
        "{}",
        "        视频 AI 放大工具 (Real-ESRGAN + FFmpeg)".bold().white()
    );
    println!("{}", "═══════════════════════════════════════════════════════════".cyan());
    println!();

    println!(
        "  {} 工作目录(程序所在目录): {}",
        "🗂".bold(),
        exe_dir.display().to_string().cyan()
    );
    println!(
        "  {} 默认输出目录: {}",
        "📁".bold(),
        default_output_dir.display().to_string().cyan()
    );

    // 显示 ffmpeg 版本
    let ffmpeg_ver = ffmpeg_version(&ffmpeg);
    match &ffmpeg_ver {
        Some(ver) => println!("  {} {}", "🎬".bold(), ver.green()),
        None => println!(
            "  {} ffmpeg 未找到（路径: {}）",
            "✖".red().bold(),
            ffmpeg.red()
        ),
    }

    // 显示 realesrgan 状态
    let realesrgan_ok = exe_exists(&realesrgan);
    if realesrgan_ok {
        println!(
            "  {} realesrgan-ncnn-vulkan: {} ({})",
            "✔".green().bold(),
            "已找到".green(),
            realesrgan.bright_black()
        );
    } else {
        println!(
            "  {} realesrgan-ncnn-vulkan 未找到（路径: {}）",
            "✖".red().bold(),
            realesrgan.red()
        );
    }

    // 必要工具缺失时直接退出，不执行后续流程
    let mut missing: Vec<String> = Vec::new();
    if ffmpeg_ver.is_none() {
        missing.push(format!("ffmpeg（路径: {}）", ffmpeg));
    }
    if !realesrgan_ok {
        missing.push(format!("realesrgan-ncnn-vulkan（路径: {}）", realesrgan));
    }
    if !missing.is_empty() {
        bail!(
            "以下必要工具未找到，无法继续运行：\n  - {}\n请检查 config.yaml 中的路径配置。",
            missing.join("\n  - ")
        );
    }

    println!();
    println!("{}", "──────────────────────────────────────────────────".bright_black());
    println!();

    // ── 用户输入 ──────────────────────────────
    let input_video_str = prompt("请输入视频文件路径 (可直接拖入文件，相对路径基于工作目录): ")?;
    if input_video_str.is_empty() {
        bail!("视频路径不能为空");
    }
    let input_video = resolve_path(&exe_dir, &input_video_str);
    if !input_video.exists() {
        bail!("视频文件不存在: {}", input_video.display());
    }

    let output_dir_str = prompt(&format!(
        "请输入视频放大输出目录 ({}): ",
        default_output_dir.display()
    ))?;
    let output_dir = if output_dir_str.is_empty() {
        default_output_dir.clone()
    } else {
        resolve_path(&exe_dir, &output_dir_str)
    };
    println!(
        "  {} 输出目录: {}",
        "📁".bold(),
        output_dir.display().to_string().cyan()
    );

    // 确保输出目录存在
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("无法创建输出目录: {}", output_dir.display()))?;

    // 文件名（不含路径和扩展名）
    let filename = input_video
        .file_stem()
        .context("无法获取文件名")?
        .to_string_lossy()
        .into_owned();

    println!();

    // ── 临时目录 ──────────────────────────────
    let input_tmp = exe_dir.join("input_image_tmp");
    let output_tmp = exe_dir.join("out_image_tmp");

    for (dir, label) in [(&input_tmp, "input_image_tmp"), (&output_tmp, "out_image_tmp")] {
        if dir.exists() {
            bail!(
                "文件夹 \"{}\" 已存在，请先处理该文件夹后再运行。",
                label
            );
        }
        fs::create_dir_all(dir)
            .with_context(|| format!("无法创建目录: {}", dir.display()))?;
    }

    // ── Step 1: FFmpeg 导出帧 ─────────────────
    println!(
        "{} {}",
        "[Step 1]".cyan().bold(),
        "正在导出视频帧，请稍候...".bold()
    );

    let frame_pattern = input_tmp.join("frame%08d.png");
    let status = Command::new(&ffmpeg)
        .current_dir(&exe_dir)
        .args([
            "-i",
            input_video.to_str().unwrap(),
            "-qscale:v",
            "1",
            "-qmin",
            "1",
            "-qmax",
            "1",
            "-vsync",
            "0",
            frame_pattern.to_str().unwrap(),
        ])
        .status()
        .with_context(|| format!("启动 ffmpeg 失败，请检查路径: {}", ffmpeg))?;

    if !status.success() {
        bail!("FFmpeg 导出帧失败（退出码: {:?}）", status.code());
    }

    let total_frames = count_files_in_dir(&input_tmp);
    println!(
        "  {} 共导出 {} 帧",
        "✔".green().bold(),
        total_frames.to_string().cyan()
    );
    println!();

    // ── Step 2: Real-ESRGAN 放大（内联进度条）──
    // 验证配置值
    let valid_models = [
        "realesr-animevideov3",
        "realesrgan-x4plus",
        "realesrgan-x4plus-anime",
        "realesrnet-x4plus",
    ];
    if !valid_models.contains(&cfg.realesrgan_model.as_str()) {
        bail!(
            "不支持的模型名 \"{}\", 可选值: {}",
            cfg.realesrgan_model,
            valid_models.join(" | ")
        );
    }
    if ![2u32, 3, 4].contains(&cfg.realesrgan_scale) {
        bail!(
            "不支持的放大倍数 {}, 可选值: 2 | 3 | 4",
            cfg.realesrgan_scale
        );
    }
    let scale_str = cfg.realesrgan_scale.to_string();

    println!(
        "{} {}",
        "[Step 2]".cyan().bold(),
        format!(
            "正在调用 Real-ESRGAN 进行 AI 放大 ({}x, 模型: {})...",
            cfg.realesrgan_scale, cfg.realesrgan_model
        ).bold()
    );

    // 设置进度条
    let pb = ProgressBar::new(total_frames as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "  [{bar:45.cyan/blue}] {pos}/{len} ({percent}%)  {msg}",
        )
        .unwrap()
        .progress_chars("█▉▊▋▌▍▎▏ "),
    );
    pb.set_message("处理中...");

    // 启动 realesrgan 子进程（后台，不独占终端）
    let mut child = Command::new(&realesrgan)
        .current_dir(&exe_dir)
        .args([
            "-i",
            input_tmp.to_str().unwrap(),
            "-o",
            output_tmp.to_str().unwrap(),
            "-n",
            cfg.realesrgan_model.as_str(),
            "-s",
            scale_str.as_str(),
            "-f",
            "png",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "启动 realesrgan-ncnn-vulkan 失败，请检查路径: {}",
                realesrgan
            )
        })?;

    // 主线程轮询进度
    loop {
        let done = count_files_in_dir(&output_tmp);
        pb.set_position(done as u64);

        match child.try_wait() {
            Ok(Some(exit_status)) => {
                // 子进程已退出
                pb.set_position(count_files_in_dir(&output_tmp) as u64);
                pb.finish_with_message("完成！");
                if !exit_status.success() {
                    bail!(
                        "Real-ESRGAN 处理失败（退出码: {:?}）",
                        exit_status.code()
                    );
                }
                break;
            }
            Ok(None) => {
                // 子进程仍在运行
                thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                pb.abandon_with_message("监控出错");
                bail!("等待 Real-ESRGAN 子进程时出错: {}", e);
            }
        }
    }

    println!();

    // ── Step 3: ffprobe 检测帧率 ─────────────
    println!(
        "{} {}",
        "[Step 3]".cyan().bold(),
        "正在检测原始视频帧率...".bold()
    );

    let fps_output = Command::new(&ffprobe)
        .current_dir(&exe_dir)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=r_frame_rate",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            input_video.to_str().unwrap(),
        ])
        .output()
        .with_context(|| format!("启动 ffprobe 失败，请检查路径: {}", ffprobe))?;

    if !fps_output.status.success() {
        bail!("ffprobe 检测帧率失败");
    }

    let fps_raw = String::from_utf8_lossy(&fps_output.stdout)
        .trim()
        .to_string();
    if fps_raw.is_empty() {
        bail!("ffprobe 未能获取帧率信息");
    }

    println!("  {} 检测到帧率: {}", "✔".green().bold(), fps_raw.cyan());
    println!();

    // ── Step 4: FFmpeg 合成最终视频 ──────────
    println!(
        "{} {}",
        "[Step 4]".cyan().bold(),
        "正在合成最终视频并压制...".bold()
    );

    let out_frame_pattern = output_tmp.join("frame%08d.png");
    let out_file = output_dir.join(format!("out_{}.mp4", filename));

    let status = Command::new(&ffmpeg)
        .current_dir(&exe_dir)
        .args([
            "-framerate",
            &fps_raw,
            "-i",
            out_frame_pattern.to_str().unwrap(),
            "-i",
            input_video.to_str().unwrap(),
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-c:a",
            "copy",
            "-c:v",
            "libx264",
            "-r",
            &fps_raw,
            "-pix_fmt",
            "yuv420p",
            out_file.to_str().unwrap(),
        ])
        .status()
        .with_context(|| "启动 ffmpeg 合成失败".to_string())?;

    if !status.success() {
        bail!("FFmpeg 合成视频失败（退出码: {:?}）", status.code());
    }

    // ── 完成 ──────────────────────────────────
    println!();
    println!("{}", "══════════════════════════════════════════════════".green());
    println!(
        "  {} 处理完成！",
        "🎉".bold()
    );
    println!(
        "  {} 输出文件: {}",
        "📄".bold(),
        out_file.display().to_string().cyan().bold()
    );
    println!("{}", "══════════════════════════════════════════════════".green());

    // 清理临时目录
    cleanup(&[&input_tmp, &output_tmp]);
    println!("  {} 临时文件已清理。", "🗑".bold());

    println!();
    println!("按 Enter 键退出...");
    let _ = prompt("");

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!();
        eprintln!("{} {}", "✖ 错误:".red().bold(), e);
        // 打印完整错误链
        let mut source = e.source();
        while let Some(cause) = source {
            eprintln!("  {} {}", "→".red(), cause);
            source = cause.source();
        }
        eprintln!();
        eprint!("按 Enter 键退出...");
        let _ = io::stdin().read_line(&mut String::new());
        std::process::exit(1);
    }
}
