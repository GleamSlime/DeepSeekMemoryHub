# lanlock —— SSH / 屏幕共享 IP 白名单

**它解决什么**：这台机器（192.168.3.246）的 22 和 5900 通过路由器端口转发暴露在公网，
每天都在被扫。sshd 和 macOS 屏幕共享**自身都没有 IP 白名单功能**，所以白名单只能在
包过滤层做：用 pf 把「非白名单来源」对 22/5900 的新连接直接丢包。

```
外部/内网 ──► 路由器端口转发 ──► [ pf 锚点 com.bluewhale.lanlock ] ──► sshd / screensharingd
                                    ├─ 白名单内：放行
                                    └─ 其余：丢包（连接根本建立不起来）
```

> 注意：这只是**第二道闸**。第一道、也是最有效的一道，是**路由器上根本不要转发 22/5900**。
> 见文末「更彻底的做法」。

## 安装

```bash
sudo /Users/gleamslime/MemoryHub/ops/lanlock/install.sh
```

装完会有：

| 路径 | 作用 |
| --- | --- |
| `/usr/local/etc/lanlock/allow.txt` | 白名单，一行一个 IP/CIDR，`#` 注释 |
| `/usr/local/etc/lanlock/lanlock.pf` | 由脚本生成，别手改 |
| `/Library/LaunchDaemons/com.bluewhale.lanlock.plist` | 开机加载 + 监听 allow.txt 变化 |
| `/etc/pf.conf` | 追加了三行 `anchor` / `load anchor`（原本是空文件） |

## 日常使用

```bash
sudo /usr/local/etc/lanlock/lanlock.sh add 1.2.3.4      # 加一条并立刻生效
sudo /usr/local/etc/lanlock/lanlock.sh remove 1.2.3.4
sudo /usr/local/etc/lanlock/lanlock.sh status           # 看白名单 + pf 状态 + 生效规则
sudo /usr/local/etc/lanlock/lanlock.sh off              # 紧急恢复：清空规则，22/5900 全放行
```

在外面临时要用、又不知道自己的出口 IP：

```bash
sudo /usr/local/etc/lanlock/lanlock.sh add "$(curl -s ifconfig.me)"
```

## 验证

```bash
sudo pfctl -s info | head -1                 # Status: Enabled
sudo pfctl -a com.bluewhale.lanlock -sr      # 应看到 table + pass + block 三条
sudo pfctl -a com.bluewhale.lanlock -t lanlock_allow -T show   # 展开后的允许列表
```

真测：从**不在白名单**的网络（比如手机 4G）连 `ssh` 或 VNC，应该直接超时（不是密码错误）。

## 出问题时

白名单最容易把人锁在外面，所以按顺序记：

1. 局域网内（192.168.3.x）**永远放行**，同网段的另一台机子还能连。
2. `sudo /usr/local/etc/lanlock/lanlock.sh off` 一条命令恢复全放行。
3. 彻底卸载：
   ```bash
   sudo launchctl bootout system/com.bluewhale.lanlock
   sudo rm /Library/LaunchDaemons/com.bluewhale.lanlock.plist
   sudo pfctl -a com.bluewhale.lanlock -F all
   sudo rm -rf /usr/local/etc/lanlock
   # 再从 /etc/pf.conf 删掉 lanlock 那三行
   ```

## 配套的加固（建议一起做）

**1. SSH 只允许密钥，关掉密码登录**（把爆破的价值降到零，且不影响白名单）

```bash
sudo tee /etc/ssh/sshd_config.d/60-lanlock.conf >/dev/null <<'EOF'
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
EOF
sudo sshd -t && sudo launchctl kickstart -k system/com.openssh.sshd
```

⚠️ 动手前先确认自己的公钥已经在 `~/.ssh/authorized_keys` 里，并且**另开一个窗口**验证能登进来再关掉旧窗口。

**2. 屏幕共享关掉弱协议**（这台机现在 `VNCLegacyConnectionsEnabled=1`、`allowInsecureDH=1`）

```bash
sudo defaults write /Library/Preferences/com.apple.RemoteManagement VNCLegacyConnectionsEnabled -bool false
sudo defaults write /Library/Preferences/com.apple.RemoteManagement allowInsecureDH -bool false
sudo launchctl kickstart -k system/com.apple.screensharing
```

**3. 僵尸会话**（这次查到的那个挂了 6 天的 ScreensharingAgent）

```bash
killall ScreensharingAgent     # 立刻回收，下次真连会自动拉起
```
不用远程桌面时，直接把「系统设置 → 通用 → 共享 → 屏幕共享」关掉最省事。

## 更彻底的做法：别开端口转发

白名单管的是「谁能连进来」，但它仍然让服务暴露在公网。真正干净的是让这台机器**没有入站端口**：

1. 路由器上删掉 22 / 5900 的端口转发；
2. 装 [Tailscale](https://tailscale.com/)（免费版够用），两台机器加入同一 tailnet；
3. 以后 `ssh gleamslime@100.x.y.z`、屏幕共享连 tailnet 地址。

好处：TCP 出站建隧道，公网扫不到任何端口；自带设备级鉴权和 ACL。代价是多一个依赖服务。

## 已知边界

- pf 规则**不持久**：重启由 LaunchDaemon 重新加载，所以别把 plist 删了。
- 与 macOS「应用防火墙」（系统设置 → 网络 → 防火墙）并存不冲突；本锚点只管 22/5900，其它流量一律不碰。
- 白名单按**来源 IP** 生效，跟用户无关；换网络的动态 IP 需要重新 `add`。
- 本仓库的 git 里还有另一个会话未提交的改动，本目录是独立的 `ops/` 而非服务代码，可单独 commit 或移出仓库。
