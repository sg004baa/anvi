; anvi のインストーラ（Inno Setup 6）。
;
;   ISCC.exe /DAppVersion=x.y.z /DStageDir=<配布物を並べたディレクトリ> installer\anvi.iss
;
; StageDir は scripts/make-bundle.sh が組み立てたレイアウト（anvi.exe /
; runtime/ / nvim/）をそのまま指す。ここで並べ替えはしない。

#ifndef AppVersion
  #error AppVersion must be provided with /DAppVersion=x.y.z
#endif

#ifndef StageDir
  #error StageDir must be provided with /DStageDir=path
#endif

[Setup]
AppId={{87C33B27-95D8-48EE-B45F-5B4B6C1622B0}
AppName=anvi
AppVersion={#AppVersion}
AppPublisher=sg004baa
AppSupportURL=https://github.com/sg004baa/anywhere-nvim
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
DefaultDirName={localappdata}\Programs\anvi
DefaultGroupName=anvi
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
UninstallDisplayIcon={app}\anvi.exe
; 常駐プロセスは WM_CLOSE で終わらない（× はセッション破棄であってアプリ終了ではない
; → DESIGN 7.2）。再起動マネージャに任せると「閉じられません」で止まるので、
; インストーラ側から明示的に殺す（下の KillResident）。
CloseApplications=no
RestartApplications=no
Compression=lzma2
SolidCompression=yes
OutputDir=..\dist
OutputBaseFilename=anvi-v{#AppVersion}-windows-x64-setup
WizardStyle=modern
; exe に埋め込んでいるものと同じアイコン（出典は scripts/make-icon.py）。
SetupIconFile=..\assets\anvi.ico
; デュアルライセンスなので、ウィザードでは MIT を出し、Apache-2.0 は
; インストール先（{app}\LICENSE-APACHE）に置く。
LicenseFile=..\LICENSE-MIT

[Tasks]
Name: "startup"; Description: "Windows へのサインイン時に自動起動する"; GroupDescription: "常駐:"

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{userprograms}\anvi"; Filename: "{app}\anvi.exe"; WorkingDir: "{app}"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "anvi"; ValueData: """{app}\anvi.exe"""; Flags: uninsdeletevalue; Tasks: startup

[Run]
Filename: "{app}\anvi.exe"; Description: "anvi を起動する"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent

[Code]
var
  // usPostUninstall で参照する、ユーザーデータ削除を実行するかどうかのフラグ。
  // Pascal Script のグローバル変数はデフォルトで False に初期化されるが、
  // InitializeUninstall で必ず明示代入するため、その暗黙初期値には依存しない。
  DeleteUserData: Boolean;

// 常駐中の anvi を落とす。/T で子の nvim.exe も巻き込む。
// ユーザーの他の nvim.exe を巻き込まないよう、イメージ名 nvim.exe は指定しない。
// 対象が居なければ taskkill は 128 を返す。これは正常系なので握りつぶす。
procedure KillResident;
var
  ResultCode: Integer;
begin
  Exec(
    ExpandConstant('{sys}\taskkill.exe'), '/F /T /IM anvi.exe', '',
    SW_HIDE, ewWaitUntilTerminated, ResultCode);
  // 終了直後はまだファイルハンドルが残っていることがある。
  Sleep(500);
end;

// コマンドライン引数に /KEEPDATA (大文字小文字無視) があるかを調べる。
// ParamCount / ParamStr は Setup と Uninstall の両方で使える組み込み関数。
function HasKeepDataParam: Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
  begin
    if CompareText(ParamStr(I), '/KEEPDATA') = 0 then
    begin
      Result := True;
      Exit;
    end;
  end;
end;

// 上書きインストールの直前に常駐を落とす。生きたままだと exe を置き換えられない。
function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  KillResident;
  Result := '';
end;

// アンインストール開始時に常駐を落とし、ユーザーデータを削除するかどうかを
// 一度だけ判定する。デフォルトは常に「削除する」(opt-out)。
//   - /KEEPDATA が付いていれば確認なしで保持する (引数優先)。
//   - silent アンインストール (UninstallSilent = True) で /KEEPDATA が
//     なければ削除する。
//   - 対話アンインストールで /KEEPDATA がなければ MsgBox(MB_YESNO,
//     デフォルトボタンは第1ボタン = はい) で確認する。
function InitializeUninstall: Boolean;
begin
  Result := True;
  KillResident;
  if HasKeepDataParam then
  begin
    DeleteUserData := False;
  end
  else if UninstallSilent then
  begin
    DeleteUserData := True;
  end
  else
  begin
    DeleteUserData :=
      MsgBox(
        'ローカル設定 (' + ExpandConstant('{localappdata}') + '\anvi\init.lua など) と' + #13#10 +
        'shada / state (' + ExpandConstant('{localappdata}') + '\anvi-data) も削除しますか? (既定: はい)' + #13#10 +
        '「いいえ」を選ぶとこれらのフォルダーは残ります。自分で書いた設定があるならこちら。',
        mbConfirmation, MB_YESNO) = IDYES;
  end;
end;

// 指定ディレクトリを再帰削除する。存在しなければ何もしない (正常系)。
// 失敗時は握りつぶさず Log に記録し、対話時は MsgBox でも通知する。
// Inno のアンインストーラは任意の終了コードを返せないため、silent 時に
// 失敗を呼び出し元へ伝える手段は Log のみとなる。
procedure DeleteDataDir(const Dir: String);
begin
  if not DirExists(Dir) then
    Exit;

  if not DelTree(Dir, True, True, True) then
  begin
    Log('anvi: failed to delete user data directory: ' + Dir);
    if not UninstallSilent then
    begin
      MsgBox(
        'ユーザーデータの削除に失敗しました: ' + Dir + #13#10 +
        '手動で削除してください。',
        mbError, MB_OK);
    end;
  end;
end;

// ファイル本体の削除が終わった後 (usPostUninstall) にユーザーデータを削除する。
// %LOCALAPPDATA%\Programs\anvi (インストール先) には一切触れない。
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if (CurUninstallStep = usPostUninstall) and DeleteUserData then
  begin
    DeleteDataDir(ExpandConstant('{localappdata}\anvi'));
    DeleteDataDir(ExpandConstant('{localappdata}\anvi-data'));
  end;
end;
