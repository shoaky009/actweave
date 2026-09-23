# ActWeave

通用 Rust 任务编排框架：模型选择 Skill，Core 调度执行，Adapter 对接具体应用。内置 Demo 和 actweave CLI，支持按需加载 Skill、本地重复执行、任务控制与可选文件日志。

## 运行

交互界面：`cargo run -p actweave-tui`。依次选择适配器、任务或功能、参数，然后在任务页查看日志和 State。功能任务不需要模型密钥；自然语言任务沿用 `JEVKEY`。任务页支持暂停、继续、取消。

安装当前 stable Rust，在项目根目录执行：

```bash
read -rs -p 'JEVKEY: ' JEVKEY
export JEVKEY
cargo run --locked -- '切换到训练模式'
```

默认使用 JEV，端点为 `https://api.typesafe.ai/v1/systemone`，模型为 `jev-latest`。通过 `--endpoint` / `JEV_ENDPOINT`、`--model` / `JEV_MODEL` 覆盖配置。密钥从 `JEVKEY` 读取，不自动加载 `.env`。协议见 [TypeSafe API 文档](https://docs.typesafe.ai/api)。

无需密钥的手动 Demo：

```bash
printf '%s\n' \
  '{"Execute":{"actions":[{"Call":{"name":"set_mode","arguments":{"mode":"training"}}}],"then":"Decide"}}' \
  '{"Completed":"当前模式已是 training"}' \
  | cargo run --locked -- --agent manual '切换到训练模式'
```

其他运行示例：

```bash
# 按需加载 Skill
cargo run --locked -- --skill-mode on-demand '启动训练并完成一次训练'
# 一次决策下达重复任务，由 Core 本地执行
cargo run --locked -- --max-decisions 1 '完成10次试验动作，不要求每次都获得结果'
# 模拟动作异常与恢复
cargo run --locked -- --scenario batch-interrupted '完成10次试验动作'
# 查看全部参数
cargo run --locked -- --help
```

## 日志与控制

终端默认输出中文 INFO，展示 Skill 名称、参数和执行结果。日志保存在 `logs/tasks/<task_id>/`，可用 `--log-dir` 修改目录。

`--log-level debug` 同时在 stderr 和任务目录的 `decisions.jsonl` 记录模型输入输出；`--log-file <路径>` 将模型日志改为写入指定文件。日志不记录鉴权头或密钥，但会包含任务和状态正文。

CLI 支持 Ctrl+C 取消任务；暂停、继续和状态查询通过 Rust API 提供。目前支持进程内继续执行，不支持重启恢复。

## 开发

模块职责、代码路径与测试入口见 [llms.txt](llms.txt)。

```bash
cargo fmt --all --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```
