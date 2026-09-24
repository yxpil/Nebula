; ============================================================================
; Nebula Windows 安装程序 (NSIS 3.x)
;
; 产出: 单个 nebula-setup-<版本>.exe,用户可自定义安装盘符与目录、
;       创建开始菜单快捷方式、写入"添加/删除程序"卸载信息;
;       卸载时完整清理程序文件(用户数据库 .ndb 不在安装目录,不会被删)。
;
; 零第三方插件依赖(仅 NSIS 自带 MUI2 / StrFunc)。
;
; 编译(路径均相对仓库根目录):
;   makensis /V2 /DVERSION=0.1.0-ES installer/nebula.nsi
;
; 前置文件(由 CI / 本地打包准备到 installer/stage/):
;   stage/nebula.exe  stage/README.md  stage/LICENSE  stage/icon.png
; ============================================================================

Unicode true

!ifndef VERSION
  !define VERSION "0.1.0-ES"
!endif

!define PRODUCT "Nebula"
!define COMPANY "yxpil"
!define EXE "nebula.exe"

Name "${PRODUCT}"
OutFile "..\dist\nebula-setup-${VERSION}.exe"
Var StartMenuFolder
InstallDir "$LOCALAPPDATA\Programs\Nebula"
InstallDirRegKey HKCU "Software\${PRODUCT}" "InstallDir"
RequestExecutionLevel user
ShowInstDetails show
ShowUnInstDetails show

!include "MUI2.nsh"

!define MUI_ICON "..\assets\nebula.ico"
!define MUI_UNICON "..\assets\nebula.ico"
!define MUI_ABORTWARNING

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_STARTMENU Application $StartMenuFolder
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "立即运行 Nebula"
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

; ----------------------------------------------------------------------------
; 原生字符串工具(零插件):
;   StrFind         —— 栈: Push 字符串, Push 子串, Call → Pop "1"/"0"
;   un.RemoveNeedle —— 栈: Push 字符串, Push 子串, Call → Pop 结果(移除首个匹配)
; ----------------------------------------------------------------------------
Function StrFind
  Pop $R1
  Pop $R0
  StrLen $R2 $R1
  StrCpy $R3 0
  sf_loop:
    StrCpy $R4 $R0 $R2 $R3
    StrCmp $R4 $R1 sf_yes
    StrCmp $R4 "" sf_no
    IntOp $R3 $R3 + 1
    Goto sf_loop
  sf_yes:
    Push "1"
    Return
  sf_no:
    Push "0"
FunctionEnd

Function un.RemoveNeedle
  Pop $R1
  Pop $R0
  StrLen $R2 $R1
  StrCpy $R3 0
  rn_loop:
    StrCpy $R4 $R0 $R2 $R3
    StrCmp $R4 $R1 rn_hit
    StrCmp $R4 "" rn_miss
    IntOp $R3 $R3 + 1
    Goto rn_loop
  rn_hit:
    StrCpy $R5 $R0 $R3
    IntOp $R7 $R3 + $R2
    StrCpy $R8 $R0 "" $R7
    StrCpy $R9 "$R5$R8"
    Push $R9
    Return
  rn_miss:
    Push $R0
FunctionEnd

; ----------------------------------------------------------------------------
Section "Nebula 核心程序(必选)" SecCore
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "stage\${EXE}"
  File "stage\README.md"
  File "stage\LICENSE"
  File "stage\icon.png"

  !insertmacro MUI_STARTMENU_WRITE_BEGIN Application
    CreateDirectory "$SMPROGRAMS\$StartMenuFolder"
    CreateShortcut "$SMPROGRAMS\$StartMenuFolder\${PRODUCT}.lnk" \
      "$INSTDIR\${EXE}" "" "$INSTDIR\${EXE}" 0
    CreateShortcut "$SMPROGRAMS\$StartMenuFolder\卸载 ${PRODUCT}.lnk" \
      "$INSTDIR\uninstall.exe"
  !insertmacro MUI_STARTMENU_WRITE_END

  WriteUninstaller "$INSTDIR\uninstall.exe"

  WriteRegStr HKCU "Software\${PRODUCT}" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT}" \
    "DisplayName" "${PRODUCT} (本地优先的个人记忆检索引擎)"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT}" \
    "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT}" \
    "DisplayIcon" "$INSTDIR\${EXE}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT}" \
    "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT}" \
    "Publisher" "${COMPANY}"
SectionEnd

; ----------------------------------------------------------------------------
Section "加入用户 PATH(可在命令行直接运行 nebula)" SecPath
  ReadRegStr $0 HKCU "Environment" "Path"
  Push $0
  Push "$INSTDIR"
  Call StrFind
  Pop $2
  StrCmp $2 "1" donePath
  StrCmp $0 "" doPlain
    WriteRegExpandStr HKCU "Environment" "Path" "$0;$INSTDIR"
    Goto notifyPath
  doPlain:
    WriteRegExpandStr HKCU "Environment" "Path" "$INSTDIR"
  notifyPath:
  ; 通知新进程 PATH 已变更(不强制重启资源管理器)
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
  donePath:
SectionEnd

; ----------------------------------------------------------------------------
Function un.onInit
  MessageBox MB_OKCANCEL|MB_ICONQUESTION \
    "确定要完全卸载 ${PRODUCT} 吗?$\n$\n程序文件将被删除;你创建的数据库文件(.ndb)不在安装目录内,不会被删除。" \
    /SD IDOK IDOK +2
  Abort
FunctionEnd

Section "Uninstall"
  ; 从用户 PATH 移除安装目录(依次处理三种位置形态,最多出现一次)
  ReadRegStr $0 HKCU "Environment" "Path"
  Push $0
  Push ";$INSTDIR"
  Call un.RemoveNeedle
  Pop $0
  Push $0
  Push "$INSTDIR;"
  Call un.RemoveNeedle
  Pop $0
  Push $0
  Push "$INSTDIR"
  Call un.RemoveNeedle
  Pop $0
  WriteRegExpandStr HKCU "Environment" "Path" "$0"
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000

  !insertmacro MUI_STARTMENU_GETFOLDER Application $StartMenuFolder
  Delete "$SMPROGRAMS\$StartMenuFolder\${PRODUCT}.lnk"
  Delete "$SMPROGRAMS\$StartMenuFolder\卸载 ${PRODUCT}.lnk"
  RMDir "$SMPROGRAMS\$StartMenuFolder"

  Delete "$INSTDIR\${EXE}"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\icon.png"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT}"
  DeleteRegKey HKCU "Software\${PRODUCT}"
SectionEnd
