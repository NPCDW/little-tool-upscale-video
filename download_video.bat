@echo off
setlocal enabledelayedexpansion

:: 设置颜色和标题
title 视频下载 yt-dlp 脚本
echo ==================================================
echo         视频下载 yt-dlp 脚本
echo ==================================================

:: 1. 让用户输入视频文件地址和输出目录
set /p input_video_url="请输入视频下载地址: "
set /p output_video_name="请输入下载后的文件名: "

:: 2. 输入下载片段（可选）
echo.
echo 提示：直接按回车将下载完整视频。
echo 如果只需片段，请输入格式如: 00:01:00-00:02:15   inf 表示视频结束
set /p time_range="请输入片段范围: "

:: 3. 构造参数
set "clip_param="
if not "%time_range%"=="" (
    set "clip_param=--download-sections "*%time_range%""
)

:: 执行下载
echo 正在开始下载...
yt-dlp.exe %input_video_url% %clip_param% -o "%output_video_name%.%%(ext)s" --downloader ffmpeg --downloader-args "ffmpeg:-map 0" --user-agent "Yamby/1.5.5.11(Android)"

if %ERRORLEVEL% equ 0 (
    echo.
    echo 下载完成！
) else (
    echo.
    echo 下载过程中出现错误，请检查链接或时间格式。
)

pause