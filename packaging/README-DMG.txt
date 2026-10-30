蓝鲸记忆中枢 — 安装说明
========================

这是什么
--------
一套跨渠道的 AI 记忆服务：让 Minecraft 里的 NPC、QQ 群机器人、直播弹幕机器人
共用同一份记忆，彼此记得对方渠道发生过的事。


安装（二选一）
--------------
A. 命令行安装（推荐，会装成开机自启的服务）

   把「蓝鲸记忆中枢.app」拖到「应用程序」，
   然后打开终端执行：

     /Applications/蓝鲸记忆中枢.app/Contents/Resources/install.sh

   要让局域网里的其他机器访问，带上 token：

     BLUEWHALE_TOKEN=<自己编一个至少16位的字符串> \
       /Applications/蓝鲸记忆中枢.app/Contents/Resources/install.sh

B. 直接运行（不装服务，窗口关掉就停）

     /Applications/蓝鲸记忆中枢.app/Contents/MacOS/bluewhale-memory serve


首次打开被 Gatekeeper 拦住？
---------------------------
这个包**没有做代码签名**（按你的要求）。所以第一次双击会被拦。

  右键点图标 → 打开 → 在弹窗里再点「打开」

只需要做一次。之后就不会再拦。


关于 token
----------
**没有 token 时，服务强制只听本机（127.0.0.1）。**
这不是 bug，是刻意的：一个监听在局域网却没有密码的服务，
等于把所有人的记忆摊开给整个内网。

要给内网用，就设一个 token（至少 16 字符）。


数据在哪
--------
  ~/.bluewhale-memory/memory.db      记忆库
  ~/.bluewhale-memory/snapshots/     每日快照
  ~/.bluewhale-memory/service.log    运行日志


备份铁律（重要）
----------------
**绝对不要用 cp / rsync / tar 直接拷 memory.db。**

SQLite 在 WAL 模式下，已提交的数据可能还在 memory.db-wal 里没合并进主库。
直接拷主库会静默丢掉最新记录，而且不报错。
（实测：拷主库后少 5 条 / 408 条，零报错。）

正确做法是用内置命令，它走 VACUUM INTO，产出的是事务一致的快照：

  /Applications/蓝鲸记忆中枢.app/Contents/MacOS/bluewhale-memory snapshot


常用命令
--------
  doctor              检查数据、快照、WAL 状态
  snapshot            立刻拍一份快照
  rollback --to <ms>  回滚：忘掉该时刻之后的记忆
  undo --id <n>       撤销某次回滚（回滚是可撤销的）
  rollback-history    列出回滚历史
