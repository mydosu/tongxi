# 同席手机薄壳的安卓构建脚本（Windows）。
#
# 为什么不直接用 `npx tauri android build`：
#   tauri CLI 在往 jniLibs 里放 lib<name>_lib.so 时用的是符号链接，而 Windows 上
#   CreateSymbolicLinkW 需要 SeCreateSymbolicLinkPrivilege（开发者模式 / 提权终端）。
#   这台机是非提权账户、开发者模式未开，那一步必然失败。这个脚本把同一件事换成
#   「复制」：npm build -> cargo build --target <triple> -> 复制 .so 进 jniLibs ->
#   gradlew assemble<Arch><Profile>（跳过会回调 tauri CLI 的 rustBuild 任务）。
#   其余一切（NDK 工具链、Gradle、AGP、签名）都和 `tauri android build` 完全同一条路径。
#
# 用法（在 mobile/ 下）：
#   pwsh -NoProfile -File scripts/android-build.ps1                 # aarch64 debug APK
#   pwsh -NoProfile -File scripts/android-build.ps1 -Target x86_64  # 给模拟器用
#   pwsh -NoProfile -File scripts/android-build.ps1 -Release -SkipFrontend
param(
  [string]$ProjectDir,
  [ValidateSet('aarch64', 'armv7', 'i686', 'x86_64')][string]$Target = 'aarch64',
  [switch]$Release,
  [switch]$SkipFrontend
)

$ErrorActionPreference = 'Stop'

# ---- 目标三元组 / ABI / Gradle 任务里的架构名 ----
$targets = @{
  aarch64 = @{ triple = 'aarch64-linux-android';       abi = 'arm64-v8a';   arch = 'Arm64';  clang = 'aarch64-linux-android' }
  armv7   = @{ triple = 'armv7-linux-androideabi';     abi = 'armeabi-v7a'; arch = 'Arm';    clang = 'armv7a-linux-androideabi' }
  i686    = @{ triple = 'i686-linux-android';          abi = 'x86';         arch = 'X86';    clang = 'i686-linux-android' }
  x86_64  = @{ triple = 'x86_64-linux-android';        abi = 'x86_64';      arch = 'X86_64'; clang = 'x86_64-linux-android' }
}
$t = $targets[$Target]
$profile = if ($Release) { 'Release' } else { 'Debug' }
$cargoProfile = if ($Release) { 'release' } else { 'debug' }

if (-not $ProjectDir) { $ProjectDir = Split-Path $PSScriptRoot -Parent }
$root = (Resolve-Path $ProjectDir).Path
$tauriDir = Join-Path $root 'src-tauri'
$genDir = Join-Path $tauriDir 'gen\android'
if (-not (Test-Path $genDir)) {
  throw "安卓工程还没生成：先在 $root 里跑 npm run android:init"
}

# ---- 环境：优先用已有的用户级变量，缺了才回落到这台机的已知路径 ----
function Resolve-First($value, $fallbacks, $probe) {
  $candidates = @()
  if ($value) { $candidates += @($value) }
  $candidates += @($fallbacks)
  foreach ($candidate in $candidates) {
    if (-not $candidate) { continue }
    if (Test-Path (Join-Path $candidate $probe)) { return $candidate }
  }
  throw "找不到需要的目录（探针 $probe）：$($candidates -join ', ')"
}

$javaHome = Resolve-First $env:JAVA_HOME @('C:\Program Files\Eclipse Adoptium\jdk-17.0.20.101-hotspot', 'C:\Program Files\Java\latest') 'bin\java.exe'
$sdkDir = Resolve-First ($env:ANDROID_HOME, $env:ANDROID_SDK_ROOT) @('D:\Android\Sdk') 'platform-tools'
$ndkDir = $env:NDK_HOME
if (-not $ndkDir -or -not (Test-Path $ndkDir)) {
  $ndkDir = Get-ChildItem (Join-Path $sdkDir 'ndk') -Directory -ErrorAction SilentlyContinue |
    Sort-Object Name -Descending | Select-Object -First 1 -ExpandProperty FullName
}
if (-not $ndkDir) { throw "找不到 NDK：请设 NDK_HOME 或先用 sdkmanager 装 ndk" }

$env:JAVA_HOME = $javaHome
$env:ANDROID_HOME = $sdkDir
$env:ANDROID_SDK_ROOT = $sdkDir
$env:NDK_HOME = $ndkDir
$env:ANDROID_NDK_HOME = $ndkDir
$env:Path = "$javaHome\bin;$sdkDir\platform-tools;$sdkDir\cmdline-tools\latest\bin;$env:Path"
$env:CI = 'true'

Write-Host "[1/6] JDK     = $javaHome"
Write-Host "      SDK     = $sdkDir"
Write-Host "      NDK     = $ndkDir"
Write-Host "      target  = $($t.triple) ($($t.abi)), profile = $cargoProfile"

# ---- 1. 前端 ----
if (-not $SkipFrontend) {
  Write-Host '[2/6] 构建前端：npm run build'
  Push-Location $root
  try { & npm run build; if ($LASTEXITCODE -ne 0) { throw "npm run build 失败（$LASTEXITCODE）" } } finally { Pop-Location }
} else {
  Write-Host '[2/6] 跳过前端构建（-SkipFrontend）'
}

# ---- 2. 首次构建：让 tauri CLI 生成 webview 桥接的 Kotlin ----
# gen/android/app/src/main/java/<包名>/generated/（Ipc、Rust、WryActivity、TauriActivity…）只有
# tauri CLI 能正确生成（里面要按插件替换 {{class-extension}} 之类的钩子），所以第一次构建时
# 借用一下 `tauri android build`：它在「把 .so 放进 jniLibs」那步会因为符号链接权限失败，
# 但走到那里时该生成的文件都已经落盘了。生成好之后这个分支就不再进。
Write-Host '[3/6] 检查 Android 桥接代码'
$conf = Get-Content (Join-Path $tauriDir 'tauri.conf.json') -Raw | ConvertFrom-Json
$pkgPath = ($conf.identifier -split '\.') -join '\'
$generatedDir = Join-Path $genDir "app\src\main\java\$pkgPath\generated"
$tauriShim = Join-Path $root 'node_modules\.bin\tauri.cmd'
if (-not (Test-Path (Join-Path $generatedDir 'TauriActivity.kt'))) {
  Write-Host '      第一次构建：借 tauri CLI 生成（它随后会因符号链接权限停下，属预期）'
  $cliArgs = @('android', 'build', '--apk', '--ci', '--target', $Target)
  if (-not $Release) { $cliArgs += '--debug' }
  Push-Location $root
  try { & $tauriShim @cliArgs *> (Join-Path ([IO.Path]::GetTempPath()) 'tongxi-mobile-bootstrap.log') } finally { Pop-Location }
  if (-not (Test-Path (Join-Path $generatedDir 'TauriActivity.kt'))) {
    throw "tauri CLI 没能生成 Android 桥接代码，看 $(Join-Path ([IO.Path]::GetTempPath()) 'tongxi-mobile-bootstrap.log')"
  }
}

# ---- 3. 补齐 tauri CLI 平时在 gen/android 里生成的三个胶水文件 ----
# 它们都在 .gitignore 里（/tauri.settings.gradle、app/tauri.build.gradle.kts、app/tauri.properties），
# 平时由 `tauri android build` 写；我们绕过了那条命令，就得自己写（内容与 CLI 生成的一致）。
$metadata = & cargo metadata --format-version 1 --manifest-path (Join-Path $tauriDir 'Cargo.toml') 2>$null
if ($LASTEXITCODE -ne 0) { throw 'cargo metadata 失败' }
$meta = $metadata | ConvertFrom-Json

function GradlePath([string]$path) { $path -replace '\\', '\\' }

$tauriPkg = $meta.packages | Where-Object { $_.name -eq 'tauri' } | Select-Object -First 1
if (-not $tauriPkg) { throw 'cargo metadata 里没有 tauri 包' }
$tauriAndroid = Join-Path (Split-Path $tauriPkg.manifest_path -Parent) 'mobile\android'

$settings = @('// THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.')
$build = @(
  '// THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.',
  'val implementation = configurations.getByName("implementation")',
  'dependencies {',
  '  implementation("androidx.lifecycle:lifecycle-process:2.10.0")'
)

$settings += "include ':tauri-android'"
$settings += "project(':tauri-android').projectDir = new File(`"$(GradlePath $tauriAndroid)`")"
$build += '  implementation(project(":tauri-android"))'

$plugins = $meta.packages |
  Where-Object { $_.name -like 'tauri-plugin-*' -and $_.name -ne 'tauri-plugin' } |
  Sort-Object name
foreach ($plugin in $plugins) {
  $androidDir = Join-Path (Split-Path $plugin.manifest_path -Parent) 'android'
  if (-not (Test-Path (Join-Path $androidDir 'build.gradle.kts'))) { continue }
  Write-Host "      插件模块：$($plugin.name)"
  $settings += "include ':$($plugin.name)'"
  $settings += "project(':$($plugin.name)').projectDir = new File(`"$(GradlePath $androidDir)`")"
  $build += "  implementation(project(`":$($plugin.name)`"))"
}
$build += '}'

Set-Content -Path (Join-Path $genDir 'tauri.settings.gradle') -Value ($settings -join "`n") -Encoding utf8
Set-Content -Path (Join-Path $genDir 'app\tauri.build.gradle.kts') -Value ($build -join "`n") -Encoding utf8

$version = [string]$conf.version
$parts = @($version.Split('-')[0].Split('.')) + @('0', '0', '0')
$versionCode = [int]$parts[0] * 1000000 + [int]$parts[1] * 1000 + [int]$parts[2]
$props = @(
  '// THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.',
  "tauri.android.versionName=$version",
  "tauri.android.versionCode=$versionCode"
)
Set-Content -Path (Join-Path $genDir 'app\tauri.properties') -Value ($props -join "`n") -Encoding utf8

# ---- 3. Rust 交叉编译 ----
Write-Host "[4/6] cargo build --target $($t.triple)"
$ndkBin = Join-Path $ndkDir 'toolchains\llvm\prebuilt\windows-x86_64\bin'
$apiLevel = if ($conf.bundle.android.minSdkVersion) { [string]$conf.bundle.android.minSdkVersion } else { '24' }
$clang = Join-Path $ndkBin "$($t.clang)$apiLevel-clang.cmd"
$clangxx = Join-Path $ndkBin "$($t.clang)$apiLevel-clang++.cmd"
if (-not (Test-Path $clang)) { throw "NDK 里没有 $clang" }

# cc / ring / 各 build script 认的是小写三元组的 CC_/CXX_/AR_，cargo 链接器认的是大写。
$upper = $t.triple.ToUpper().Replace('-', '_')
Set-Item -Path "Env:CARGO_TARGET_${upper}_LINKER" -Value $clang
Set-Item -Path "Env:CC_$($t.triple)" -Value $clang
Set-Item -Path "Env:CXX_$($t.triple)" -Value $clangxx
Set-Item -Path "Env:AR_$($t.triple)" -Value (Join-Path $ndkBin 'llvm-ar.exe')
Set-Item -Path "Env:CFLAGS_$($t.triple)" -Value ''
Set-Item -Path "Env:CXXFLAGS_$($t.triple)" -Value ''

$cargoArgs = @('build', '--manifest-path', (Join-Path $tauriDir 'Cargo.toml'), '--target', $t.triple)
if ($Release) { $cargoArgs += '--release' }
Push-Location $root
try { & cargo @cargoArgs; if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（$LASTEXITCODE）" } } finally { Pop-Location }

# ---- 4. 复制 .so（取代 tauri CLI 的符号链接） ----
$soDir = Join-Path $tauriDir "target\$($t.triple)\$cargoProfile"
$so = Get-ChildItem $soDir -Filter '*.so' -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $so) { throw "$soDir 里没有 .so" }
$jniRoot = Join-Path $genDir 'app\src\main\jniLibs'
if (Test-Path $jniRoot) { Remove-Item $jniRoot -Recurse -Force }
$jniDir = Join-Path $jniRoot $t.abi
New-Item -ItemType Directory -Force -Path $jniDir | Out-Null
Copy-Item $so.FullName (Join-Path $jniDir $so.Name) -Force
Write-Host "[5/6] 复制 $($so.Name)（$([math]::Round($so.Length / 1MB, 1)) MB）-> jniLibs\$($t.abi)"

# ---- 5. Gradle ----
$task = "assemble$($t.arch)$profile"
$skip = "rustBuild$($t.arch)$profile"
Write-Host "[6/6] gradlew $task -x $skip"
Push-Location $genDir
try {
  & (Join-Path $genDir 'gradlew.bat') --no-daemon $task '-x' $skip
  if ($LASTEXITCODE -ne 0) { throw "gradle 失败（$LASTEXITCODE）" }
} finally { Pop-Location }

$apks = Get-ChildItem (Join-Path $genDir 'app\build\outputs\apk') -Recurse -Filter '*.apk' -ErrorAction SilentlyContinue |
  Sort-Object LastWriteTime -Descending
if (-not $apks) { throw '没找到 APK' }
Write-Host ''
foreach ($apk in $apks) { Write-Host ("APK: {0}  ({1:N1} MB)" -f $apk.FullName, ($apk.Length / 1MB)) }
