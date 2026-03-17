---
description: Deploy zeroclaw binary to OpenWrt router at 192.168.8.1
---

# Deploy ZeroClaw to Router

// turbo-all

## Prerequisites
- Router: `root@192.168.8.1` (password: use sshpass)
- Target: `aarch64-unknown-linux-musl`
- Linker: `aarch64-unknown-linux-musl-gcc`
- Service: `/etc/init.d/zeroclaw` (procd)

## Steps

1. **Get current version on router** before anything:
```bash
sshpass -p 'Kidd1611@@' ssh root@192.168.8.1 '/usr/bin/zeroclaw --version'
```
Save this version string — you will need it for backup naming.

2. **Build release binary** with size optimizations:
```bash
cd /Volumes/SSD/Developments/zeroclaw-custom
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-unknown-linux-musl-gcc \
  CARGO_PROFILE_RELEASE_OPT_LEVEL=z \
  CARGO_PROFILE_RELEASE_LTO=true \
  CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
  CARGO_PROFILE_RELEASE_STRIP=symbols \
  cargo build --release --locked --target aarch64-unknown-linux-musl
```

3. **Compress with UPX**:
```bash
cp target/aarch64-unknown-linux-musl/release/zeroclaw /tmp/zeroclaw-deploy
upx --best --lzma /tmp/zeroclaw-deploy
```

4. **SCP to router**:
```bash
sshpass -p 'Kidd1611@@' scp /tmp/zeroclaw-deploy root@192.168.8.1:/tmp/zeroclaw-new
```

5. **Stop service, backup WITH VERSION, replace, start**:
```bash
sshpass -p 'Kidd1611@@' ssh root@192.168.8.1 '
  OLD_VERSION=$(/usr/bin/zeroclaw --version | awk "{print \$2}");
  /etc/init.d/zeroclaw stop;
  sleep 2;
  cp /usr/bin/zeroclaw /usr/bin/zeroclaw.v${OLD_VERSION}.bak;
  chmod +x /tmp/zeroclaw-new;
  mv /tmp/zeroclaw-new /usr/bin/zeroclaw;
  /usr/bin/zeroclaw --version;
  /etc/init.d/zeroclaw start;
  sleep 3;
  logread -e zeroclaw | tail -5
'
```

## Rules

> [!CAUTION]
> - **NEVER** use `nohup` to start daemon — always use `/etc/init.d/zeroclaw start|stop|restart`
> - **ALWAYS** backup with version: `zeroclaw.v{VERSION}.bak` — never `zeroclaw.bak`
> - **NEVER** restart the service multiple times rapidly — Telegram messages queue up and cancel each other
> - **ALWAYS** wait for queue drain after restart before testing (watch logs for cancel storm to end)
