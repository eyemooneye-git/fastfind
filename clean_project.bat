@echo off
chcp 65001 >nul

echo ===================================
echo 清理 fastfind 项目...
echo ===================================
cd /d "c:\Projects\fastfind"
call cargo clean
echo.
echo fastfind 清理完成！
echo.



