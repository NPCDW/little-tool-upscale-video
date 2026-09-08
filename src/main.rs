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

#[derive(Debug, Serialize, Deserialize)]
struct Config {
    /// 视频放大默认存储目录，留空则使用程序所在目录
    output_dir: String,
    /// ffmpeg 可执行文件路径
    ffmpeg_path: String,
    /// ffprobe 可执行文件路径
    ffprobe_path: String,
    /// realesrgan-ncnn-vulkan 可执行文件路径
    realesrgan_path: String,
}

impl Config {
    /// 根据程序所在目录生成默认配置
    fn default_for(exe_dir: &Path) -> Self {
        let exe_str = |name: &str| -> String {
            // Windows 下附加 .exe，其他平台不加
            #[cfg(target_os = "windows")]
            let name = format!("{}.exe", name);
            #[cfg(not(target_os = "windows"))]
            let name = name.to_string();
            exe_dir.join(&name).to_string_lossy().into_owned()
        };

        Config {
            output_dir: exe_dir.to_string_lossy().into_owned(),
            ffmpeg_path: exe_str("ffmpeg"),
            ffprobe_path: exe_str("ffprobe"),
            realesrgan_path: exe_str("realesrgan-ncnn-vulkan"),
        }
    }
}

// ─────────────────────────────────────────────
// 配置文件读写
// ─────────────────────────────────────────────

const CONFIG_FILENAME: &str = "config.yaml";

fn config_comment() -> &'static str {
    r#"# 视频 AI 放大工具 配置文件
# ─────────────────────────────────────────────
# output_dir     : 视频放大默认存储目录（留空或路径不存在则使用程序所在目录）
# ffmpeg_path    : ffmpeg  可执行文件路径
# ffprobe_path   : ffprobe 可执行文件路径（通常与 ffmpeg 同目录）
# realesrgan_path: realesrgan-ncnn-vulkan 可执行文件路径
# ─────────────────────────────────────────────

"#
}

fn load_or_create_config(exe_dir: &Path) -> Result<Config> {
    let config_path = exe_dir.join(CONFIG_FILENAME);

    if !config_path.exists() {
        // 首次运行：生成默认配置
        let default_cfg = Config::default_for(exe_dir);
        let yaml = serde_yaml::to_string(&default_cfg)
            .context("序列化默认配置失败")?;
        let content = format!("{}{}", config_comment(), yaml);
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
// 辅助：检查可执行文件是否存在
// ─────────────────────────────────────────────

fn exe_exists(path: &str) -> bool {
    // 优先检查直接路径
    if Path::new(path).exists() {
        return true;
    }
    // 再用 which 风格：直接运行 --help / -version 探测
    Command::new(path)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
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
    // 确定工作目录：exe 所在目录
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

    // 解析输出目录（配置若为空则用 exe_dir）
    let default_output_dir = if cfg.output_dir.trim().is_empty() {
        exe_dir.clone()
    } else {
        PathBuf::from(&cfg.output_dir)
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
        "  {} 默认输出目录: {}",
        "📁".bold(),
        default_output_dir.display().to_string().cyan()
    );

    // 显示 ffmpeg 版本
    if let Some(ver) = ffmpeg_version(&cfg.ffmpeg_path) {
        println!("  {} {}", "🎬".bold(), ver.green());
    } else {
        println!(
            "  {} ffmpeg 未找到（路径: {}）",
            "✖".red().bold(),
            cfg.ffmpeg_path.red()
        );
    }

    // 显示 realesrgan 状态
    if exe_exists(&cfg.realesrgan_path) {
        println!(
            "  {} realesrgan-ncnn-vulkan: {}",
            "✔".green().bold(),
            "已找到".green()
        );
    } else {
        println!(
            "  {} realesrgan-ncnn-vulkan 未找到（路径: {}）",
            "✖".red().bold(),
            cfg.realesrgan_path.red()
        );
    }

    println!();
    println!("{}", "──────────────────────────────────────────────────".bright_black());
    println!();

    // ── 用户输入 ──────────────────────────────
    let input_video_str = prompt("请输入视频文件路径 (可直接拖入文件): ")?;
    if input_video_str.is_empty() {
        bail!("视频路径不能为空");
    }
    let input_video = PathBuf::from(&input_video_str);
    if !input_video.exists() {
        bail!("视频文件不存在: {}", input_video.display());
    }

    let output_dir_str = prompt(&format!(
        "请输入输出目录 (留空使用默认 {}): ",
        default_output_dir.display()
    ))?;
    let output_dir = if output_dir_str.is_empty() {
        default_output_dir.clone()
    } else {
        PathBuf::from(&output_dir_str)
    };

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
    let status = Command::new(&cfg.ffmpeg_path)
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
        .with_context(|| format!("启动 ffmpeg 失败，请检查路径: {}", cfg.ffmpeg_path))?;

    if !status.success() {
        cleanup(&[&input_tmp, &output_tmp]);
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
    println!(
        "{} {}",
        "[Step 2]".cyan().bold(),
        "正在调用 Real-ESRGAN 进行 AI 放大 (2x)...".bold()
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
    let mut child = Command::new(&cfg.realesrgan_path)
        .args([
            "-i",
            input_tmp.to_str().unwrap(),
            "-o",
            output_tmp.to_str().unwrap(),
            "-n",
            "realesr-animevideov3",
            "-s",
            "2",
            "-f",
            "png",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "启动 realesrgan-ncnn-vulkan 失败，请检查路径: {}",
                cfg.realesrgan_path
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
                    cleanup(&[&input_tmp, &output_tmp]);
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
                cleanup(&[&input_tmp, &output_tmp]);
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

    let fps_output = Command::new(&cfg.ffprobe_path)
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
        .with_context(|| format!("启动 ffprobe 失败，请检查路径: {}", cfg.ffprobe_path))?;

    if !fps_output.status.success() {
        cleanup(&[&input_tmp, &output_tmp]);
        bail!("ffprobe 检测帧率失败");
    }

    let fps_raw = String::from_utf8_lossy(&fps_output.stdout)
        .trim()
        .to_string();
    if fps_raw.is_empty() {
        cleanup(&[&input_tmp, &output_tmp]);
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

    let status = Command::new(&cfg.ffmpeg_path)
        .args([
            "-r",
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
        cleanup(&[&input_tmp, &output_tmp]);
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
