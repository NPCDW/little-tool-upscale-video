@echo off
setlocal enabledelayedexpansion

:: --- 新增：参数检测跳转 ---
if "%1"=="_monitor" goto _monitor
:: -----------------------

:: 设置颜色和标题
title 视频 AI 放大工具 (Real-ESRGAN + FFmpeg)
echo ==================================================
echo         视频 AI 放大自动化脚本
echo ==================================================

:: 1. 让用户输入视频文件地址和输出目录
set /p input_video="请输入视频文件地址 (可直接拖入文件): "
set /p output_dir="请输入输出视频目录 (可直接拖入文件夹): "

:: 去除用户输入中可能带有的引号
set "input_video=%input_video:"=%"
set "output_dir=%output_dir:"=%"

:: 获取输入文件的文件名（不含路径和扩展名）
for %%i in ("%input_video%") do (
    set "filename=%%~ni"
)

:: 2. 创建两个临时目录
if exist "input_image_tmp" (
    echo [错误] 文件夹 "input_image_tmp" 已存在。
    echo 请处理该文件夹后再运行脚本。
    pause
    exit /b
)
mkdir "input_image_tmp"
if exist "out_image_tmp" (
    echo [错误] 文件夹 "out_image_tmp" 已存在。
    echo 请处理该文件夹后再运行脚本。
    pause
    exit /b
)
mkdir "out_image_tmp"

:: 3. 执行 FFmpeg 导出视频帧
echo [Step 1] 正在导出视频帧，请稍候...
ffmpeg -i "%input_video%" -qscale:v 1 -qmin 1 -qmax 1 -vsync 0 "input_image_tmp/frame%%08d.png"

:: 4. 执行 Real-ESRGAN 转换
:: 启动一个新的 CMD 窗口执行监控逻辑
start "文件处理进度监控" cmd /c "%~f0" _monitor
echo [Step 2] 正在调用 Real-ESRGAN 进行 AI 放大 (2x)...
realesrgan-ncnn-vulkan.exe -i input_image_tmp -o out_image_tmp -n realesr-animevideov3 -s 2 -f png

:: 5. 获取原始视频帧率
echo [Step 3] 正在检测原始视频帧率...
:: 使用 ffprobe 获取帧率是最准确且适合脚本的方法
for /f "tokens=*" %%i in ('ffprobe -v error -select_streams v:0 -show_entries stream^=r_frame_rate -of default^=noprint_wrappers^=1:nokey^=1 "%input_video%"') do (
    set "fps_raw=%%i"
)

:: ffprobe 返回的可能是分数形式如 24000/1001，ffmpeg 能直接处理这种格式
echo 检测到帧率为: %fps_raw%

:: 6. 合成最终视频
echo [Step 4] 正在合成最终视频并压制...
ffmpeg -r %fps_raw% -i "out_image_tmp/frame%%08d.png" -i "%input_video%" -map 0:v:0 -map 1:a:0 -c:a copy -c:v libx264 -r %fps_raw% -pix_fmt yuv420p "%output_dir%\out_%filename%.mp4"

echo ==================================================
echo 处理完成！输出文件: "%output_dir%\out_%filename%.mp4"

:: 清理
rd /s /q "input_image_tmp"
rd /s /q "out_image_tmp"
echo 临时文件已清理。

pause
exit /b


:: --- 监控逻辑子程序 ---
:_monitor
mode con cols=60 lines=10

:: 统计 input 文件夹初始文件总数
set "TOTAL=0"
for /f %%A in ('dir /b /a-d "input_image_tmp" 2^>nul ^| find /c /v ""') do set "TOTAL=%%A"

if %TOTAL% equ 0 (
    echo [提示] "input_image_tmp" 为空，无需监控。
    pause
    exit /b
)

:loop
cls
:: 统计 output 文件夹当前文件数
set "CURRENT=0"
for /f %%A in ('dir /b /a-d out_image_tmp 2^>nul ^| find /c /v ""') do set "CURRENT=%%A"

:: 计算百分比
set /a "PERCENT=(CURRENT * 100) / TOTAL"

:: 构建简易进度条 (20个字符长度)
set /a "BAR_NUM=PERCENT / 5"
set "BAR="
for /l %%i in (1,1,%BAR_NUM%) do set "BAR=!BAR!█"
for /l %%i in (%BAR_NUM%,1,19) do set "BAR=!BAR! "

:: 显示界面
echo ============================================================
echo   文件同步进度监控
echo ============================================================
echo   总数 (Input): %TOTAL%
echo   当前 (Output): %CURRENT%
echo.
echo   进度: [%BAR%] %PERCENT% %%
echo ============================================================
echo   正在监控中...

:: 判断是否完成
if %CURRENT% geq %TOTAL% (
    echo.
    echo [完成] 数量已对齐，窗口即将关闭。
    timeout /t 2 >nul
    exit
)

timeout /t 1 >nul
goto loop