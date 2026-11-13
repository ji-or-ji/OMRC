# 提权命令托管

季走开的时候，我需要一个能执行管理员命令的落点。这就是它。

## 它做什么

一个以管理员身份常驻的 PowerShell 进程，只做一件事：

```
轮询 .admin\inbox\*.ps1  →  用 pwsh 执行  →  结果写到 .admin\outbox\
```

没有任何网络监听，不接受远程输入，只认这个目录里的文件。

## 启动

```powershell
Start-Process pwsh.exe -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','D:\project\software\OMRC\.admin\host.ps1' -Verb RunAs
```

UAC 点一次「是」，之后这个会话里的提权工作都不用再点。

## 使用

1. 往 `.admin\inbox\` 放一个 `xxx.ps1`
2. 等 `.admin\outbox\xxx.out` 出现
3. `.admin\outbox\xxx.code` 里是 `exit=<code>` 和时间戳
4. 跑过的脚本会被改名成 `xxx.done` 留档

## 收工

放一个空的 `.admin\stop` 文件，宿主会在下一轮退出（并删掉这个文件）。
另外 45 分钟没有新任务会自动退出，防止忘在那儿。

## 日志

`.admin\host.log` 记录启动、每条任务、退出。启动时会写明 `admin=True/False`，用来确认提权真的生效了。
