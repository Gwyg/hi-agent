; hi-agent 安装器钩子（由 tauri.conf.json 的 bundle.windows.nsis.installerHooks 引入）
;
; 这个文件只解决一件事：卸载时把 ~\.hi-agent 也清掉。
;
; 起因：模板自带的「删除应用数据」复选框删的是
;   $APPDATA\<bundle-id>      即 %APPDATA%\com.hiagent.app
;   $LOCALAPPDATA\<bundle-id> 即 %LOCALAPPDATA%\com.hiagent.app
; 而本程序的数据全在 ~\.hi-agent（SQLite 库、app.toml、日志），模板够不到。
; 用户勾了复选框、以为删干净了，重装后会话历史却一条不少 —— 认知错位。
;
; 判定条件与模板内置逻辑保持一致：仅在用户勾选、且不是升级流程时删除。
; 模板里这两个变量在卸载 Section 内可直接引用，此处只是补一条 RmDir。

!include LogicLib.nsh

!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    SetShellVarContext current
    RmDir /r "$PROFILE\.hi-agent"
  ${EndIf}
!macroend
