# 同席 · 手机薄壳（Android）

独立的小 Tauri 2 工程，只有「配对」一页：粘贴桌面「成员与服务 → 远程访问 → 复制访问地址」得到的
`http://100.x:47680/?token=…`，存本地后整页跳过去——跳过去之后跑的是桌面那份前端，薄壳不参与。

```bash
npm install
npm run android:init                      # 生成 src-tauri/gen/android（已提交，通常不用再跑）
npm run android:apk                       # aarch64 debug APK
npm run android:apk:x86_64                # 模拟器用的 x86_64 debug APK
```

Windows 上 `npx tauri android build` 会在「把 .so 放进 jniLibs」那步用符号链接，非提权账户必然失败，
所以构建走 `scripts/android-build.ps1`（换成复制，其余与官方命令同一条路径），细节见脚本顶部注释。
明文 http 放行在 `src-tauri/gen/android/app/src/main/res/xml/network_security_config.xml`。
