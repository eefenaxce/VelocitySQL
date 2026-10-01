# VelocitySQL

> 英文文档：[README.md](README.md)（[docs/](docs/) 亦为英文）。

内存常驻、兼容 PostgreSQL 语法的 SQL 引擎（Rust 实现）。

**当前状态：M1 完成，M3 已打通并可多库、带认证运行，持久化已落地（快照式）** —— 解析 → 类型 → 目录 → 内存存储 → 规划 → 执行 的最小闭环可用，
已实现 PostgreSQL Wire Protocol v3（`psql` 与常见驱动可直连），支持真正的 `CREATE DATABASE` / 多库隔离，
基于 **SCRAM-SHA-256** 的角色与口令认证，以及**进程退出不丢数据的后台快照**。

## 一、跑起来

```powershell
# 1) 启动服务（--demo 预置示例数据），默认 127.0.0.1:5210
cargo run -p vsql-server -- --demo

# 输出：
# VelocitySQL 0.1.0 (in-memory)
# listening on 127.0.0.1:5210  -  protocol: PostgreSQL wire v3
# authentication: scram-sha-256
# connect with:  psql -h 127.0.0.1 -p 5210 -U velocitysql

# 2) 用 psql 连接（本机装有 PostgreSQL 客户端时，会提示输入口令）
psql -h 127.0.0.1 -p 5210 -U velocitysql
```

没有 `psql` 也能用内置控制台（进程内执行，不经过网络）：

```powershell
cargo run -p vsql-cli -- --demo                                    # 交互式
cargo run -p vsql-cli -- --demo --password velocitysql -c "SELECT current_user"
```

服务端读写都在内存里完成，另有后台任务把整个集群（角色、数据库、schema、表、行、视图）
序列化到 `data/velocitysql.snapshot`：**每次写入后尽快**（会话在语句返回后触发），外加周期兜底与退出前的一次落盘。
因此重启最多丢失「仍在飞行中」的写入，而运行时 `CREATE DATABASE` / `CREATE ROLE` / `CREATE VIEW`
与自建表一样会被恢复；`--init`/`--demo` 每次启动仍先执行，快照负责重装它们没建过的对象。
文件用 `bincode` 编码（二进制、非 JSON），写入走「临时文件 + `fsync` + 原子 rename」。
这是**快照而非 WAL**：没有事务原子性、没有崩溃点恢复，属 M2。

### 日志

服务端用 `tracing`。**每条语句默认就打印**（`info` 级）——要看客户端到底发了什么，不该需要设环境变量再重启：

- **每条语句**都以 `info` 打印：简单查询、`Parse`、`Execute`（含 `$1=1, $2=...` 形式的绑定参数，
  单个值超 120 字符截断），末尾的 `elapsed=1.23ms` 是这条语句自身花的时间。
  日志在**语句跑完之后**才写（耗时只有那时才知道）。启动横幅会写明当前状态是 `statement log: on` 还是 `off`。
- **失败的语句**以 `warn` 打印，同样带 SQL、参数与 `elapsed=`（`Parse` 阶段即规划期报错也带 SQL）：
  只报 `column "x" does not exist` 而不说是哪条语句没法排查（扩展协议以前一条都不打）。
- `RUST_LOG` 可收窄：`RUST_LOG=vsql_protocol=warn` 只留失败、不打每条语句；
  `RUST_LOG=debug` 追加连接级细节（连接关闭、会话结束等）。默认过滤级别是 `info`。
- 角色 DDL 里的 `PASSWORD` 掩码成 `'***'`：`CREATE/ALTER ROLE` 的口令是**明文过线**的
  （SCRAM 保护的是登录，不是 DDL），照原样打日志等于把口令写进日志。非角色 DDL 一律不改写，
  所以 `SELECT password FROM secrets` 原样记录。

`velocitysql-cli` 的元命令来自目录 API（不依赖尚未实现的 `pg_catalog`）：

```
velocitysql=> \l             -- 列出所有数据库（* 标记当前库）
velocitysql=> \c shop        -- 切换数据库（等价于 psql 的 \c）
velocitysql=> \du            -- 列出所有角色（属性、是否有口令）
shop=>       \dt             -- 当前库的表
shop=>       \d items        -- 列、类型、可空、默认值，以及索引
shop=>       \dn \di \? \q
```

## 二、多数据库（数据库 ⊆ 集群）

一个 VelocitySQL 进程就是 PostgreSQL 意义上的**集群**：

```
Engine（进程 = 集群）
  ├── Database  "velocitysql" / "postgres" / 用户建的库
  │     └── Catalog + Storage + 视图        ← 每库各一套，互不可见
  │           └── Schema（pg_catalog / public / information_schema）
  │                 └── 表 / 索引 / 序列
  └── Role  "velocitysql" / 用户建的角色     ← 集群级，同 pg_authid
```

```sql
CREATE DATABASE shop;                    -- 同一集群内新建数据库
CREATE DATABASE IF NOT EXISTS shop;      -- 幂等
DROP DATABASE shop;                      -- 需无人连接，且不能在事务内
```

实测（服务端 + 外部协议客户端，非单元测试）：

| 场景 | 结果 |
|------|------|
| `velocitysql` 中 `SELECT count(*) FROM items` | `42P01 relation "items" does not exist` |
| `shop` 中 `SELECT count(*) FROM items` | 正常返回该库自己的数据 |
| 重复建库 | `42P04 database "shop" already exists` |
| `CREATE DATABASE pg_x` | `42939 unacceptable database name "pg_x"` |
| 有连接时 `DROP DATABASE shop` | `55006 database "shop" is being accessed by other users` |
| `DROP DATABASE velocitysql`（当前库） | `55006 cannot drop the currently open database` |
| 事务内建库 | `25001 CREATE DATABASE cannot run inside a transaction block` |
| 连接到不存在的库 | `FATAL 3D000 database "nope" does not exist`（随后关闭连接，同 PG） |
| `SELECT current_database()` | 返回实际连接的库，不再是常量 |

关键语义（与 PostgreSQL 对齐）：

- **会话与库绑定**：连接时确定，之后只能通过 `\c`/新连接切换；这就是「跨库引用不可能」而非「不推荐」的原因。
- **OID 作用域**：关系 OID 每库独立分配（`pg_class.oid` 只在库内唯一），数据库与角色 OID 集群唯一。
- **`DROP DATABASE` 双保险**：当前库拒绝、仍有会话连接时拒绝；`Session` 的 `Drop` 负责释放连接名额。
- **库名规范化**：未加引号折叠为小写；`pg_` 前缀保留给系统。超过 63 字节（PG 的 `NAMEDATALEN-1`）**直接报错**
  而不是像 PG 那样静默截断——一个叫不回去的库名比报错更糟。
- 启动时预建 `postgres`，因为大量脚本/工具/文档硬编码这个名字；`velocitysql` 为默认库。

## 三、角色与口令认证

```sql
-- 内置超级用户（引导角色）；口令用 --superuser-password 指定
CREATE USER app PASSWORD 's3cret';                 -- CREATE USER 隐含 LOGIN
CREATE ROLE job;                                   -- CREATE ROLE 默认 NOLOGIN
CREATE ROLE admin SUPERUSER CREATEDB CREATEROLE PASSWORD 'x';
CREATE ROLE IF NOT EXISTS app;
ALTER ROLE app PASSWORD 'newpass';                 -- 改口令
ALTER ROLE app PASSWORD NULL;                      -- 清除口令 = 禁止口令登录
DROP ROLE app;                                     -- 支持 IF EXISTS
```

```powershell
# 默认即口令认证（scram-sha-256），可用 --auth trust 关闭
cargo run -p vsql-server -- --demo --superuser-password 'TopSecret!'
psql -h 127.0.0.1 -p 5210 -U velocitysql           # psql 会提示输入口令
cargo run -p vsql-cli -- --password TopSecret!     # 控制台也走真实认证路径
```

除自建客户端外，握手还通过了 **node-postgres（`pg` 8.x）** 这一真实驱动的验证
（连接、查询、事务、建号后用新角色登录、权限拒绝、错误口令被拒）。
libpq 的启动状态机很挑剔：`AuthenticationOk` 之后的任何 `R` 都会被当成协议破坏，
报 `unexpected response from server; first received character was "R"`——wire 测试现在钉死了这条规则。

实测（服务端 + 独立进程里的真实 SCRAM 客户端，非单元测试）：

| 场景 | 结果 |
|------|------|
| 正确口令 | 握手完成，并**校验服务端签名**（证明服务端确实持有校验器） |
| 错误口令 / 不存在的角色 | 均为 `FATAL 28P01 password authentication failed for user "x"` |
| `CREATE USER reporter PASSWORD 'r3port'` 后登录 | 成功，`current_user` = `reporter` |
| 普通角色执行 `CREATE ROLE` | `42501 permission denied to create role` |
| 普通角色执行 `CREATE DATABASE` | `42501 permission denied to create database` |
| 角色无 `LOGIN` 属性 | `28000 role "x" is not permitted to log in` |
| 重复创建 | `42710 role "x" already exists`（`IF NOT EXISTS` 幂等） |
| `DROP ROLE` 当前角色 | `55006 current user cannot be dropped` |
| `DROP ROLE velocitysql`（引导角色） | `55006 cannot drop the bootstrap superuser` |
| `ALTER ROLE ... VALID UNTIL` / `INHERIT` / `CONNECTION LIMIT` | `feature not supported`（显式拒绝，不静默忽略） |

关键设计：

- **口令从不以明文存储，也不在网络上传输**。角色保存的是 SCRAM 校验器
  （`SCRAM-SHA-256$4096:<salt>$<StoredKey>:<ServerKey>`，即 `pg_authid.rolpassword` 的格式，salt 每角色随机）；
  认证是零知识证明：客户端证明自己知道口令，服务端回签名证明自己持有校验器。
- **抗账号枚举**：未知角色同样收到一个（诱饵）挑战，失败点与错误口令完全一致，握手过程无法用来探测账号是否存在。
- **实现经过外部向量验证**：`vsql-catalog::scram` 断言 RFC 7677 §3 公布的 client proof 与 server signature，
  避免「客户端与服务端共用同一份错误实现」而互相校验通过。
- **默认口令是文档化的**：未指定 `--superuser-password` 时使用 `velocitysql`，且启动时**打印警告**；
  默认值公开是有意的——找不到的默认值没人会去改。
- **`trust` 是显式选项**：`--auth trust` 才允许角色免口令连接，且启动时同样打印警告。
- **`CREATE USER` 需要等价改写**：`sqlparser` 把 `CREATE USER` 实现成 Snowflake 的键值形式，PG 写法直接解析失败；
  引擎在**词法层**把 `USER` 关键字替换为 `ROLE`（长度相同，不移动任何偏移；字符串字面量里的 `USER` 不会被误改），
  并让 `LOGIN` 默认为真——这正是 PostgreSQL 对 `CREATE USER` 的定义。

## 四、测试与质量门禁

```powershell
cargo test --workspace                                   # 138 项：134 单元/集成 + 4 文档测试
cargo clippy --workspace --all-targets -- -D warnings     # 零告警
cargo fmt --all -- --check
```

| 测试目标 | 数量 | 内容 |
|----------|------|------|
| `vsql-types` | 10 | 值比较、NULL 语义、类型转换、interval |
| `vsql-parser` | 13 | 标识符折叠、字面量类型、错误文本、`CREATE USER` 等价改写、角色 DDL |
| `vsql-catalog` | 13 | 命名空间、OID、约束、索引查找、**SCRAM 校验器（RFC 7677 向量）** |
| `vsql-storage` | 10 | 行存、唯一/NOT NULL 约束、索引随行移动、范围扫描 |
| `vsql-executor`（单元，含 `database`） | 13 | 三值逻辑、类型提升、LIKE、关联作用域、多库注册表 |
| `vsql-executor`（端到端 M1） | 24 | 全栈 SQL 行为对照 PostgreSQL |
| `vsql-executor`（端到端 多库） | 13 | 隔离性、`3D000`/`42P04`/`55006`/`25001`/`42939`、`\c` 语义 |
| `vsql-executor`（端到端 角色） | 12 | 口令存储与校验、`28P01`、权限拒绝、`DROP ROLE` 规则 |
| `vsql-protocol`（单元） | 3 | 文本格式与 `typlen` |
| `vsql-protocol`（wire，真实 TCP） | 20 | 握手、简单/扩展查询、错误与 SQLSTATE、事务状态、FATAL、**SCRAM 握手与凭据拒绝** |
| `vsql-cli` | 3 | 表格排版与行数页脚 |

## 五、Crate 结构

| Crate | 职责 |
|-------|------|
| `vsql-types` | 值表示、类型系统、比较/排序/哈希、转换矩阵 |
| `vsql-parser` | `sqlparser-rs`（PG 方言）解析 → 降级为自有 AST；不支持的特性显式报错 |
| `vsql-catalog` | 库/schema/表/列/索引/序列/角色的**元数据**，OID 分配，SCRAM 口令校验器 |
| `vsql-storage` | 行存（按 `RowId` 寻址）+ B-Tree / Hash 索引 + 约束校验 |
| `vsql-executor` | 多库容器（`Engine`/`Database`）+ 角色注册表与认证 + 规划器 + 火山模型执行器 + 会话 + 异步快照持久化（`persist`） |
| `vsql-protocol` | PostgreSQL Wire Protocol v3 编解码 + SCRAM 服务端握手 + 按库/按角色路由 |
| `vsql-server` | 服务端二进制：监听、会话、认证方式与引导口令配置 |
| `vsql-cli` | 内嵌控制台与管理工具（`\l`/`\c`/`\du`/`\dn`/`\dt`/`\di`/`\d`） |

> `DatabaseDef` / `RoleDef`（元数据）在 `vsql-catalog`，而「拥有一个库/一套角色」在 `vsql-executor`：
> 因为持有 `Storage` 就意味着依赖 `vsql-storage`，而后者依赖 `vsql-catalog`——组合的职责只能在更上层。
> 规划器（`planner`/`plan`/`explain`）暂居 `vsql-executor`，M5 引入成本模型后再独立成 crate。
> `vsql-txn` / `vsql-wal`（M2）尚未创建。

## 六、设计要点

**解析与降级分离。** `sqlparser-rs` 只负责「看得懂」，`lower` 负责「认不认」。引擎尚不支持的构造
（窗口函数、`ON CONFLICT`、`LATERAL`、`CHECK`、`PARTITION BY`、角色成员关系 …）一律在降级阶段报
`feature not supported: …`（`0A000`），绝不静默忽略子句。

**标识符规范化只做一次。** 未加引号折叠为小写、加引号保留原样，之后各层只比较 `String`。

**规划期绑定，执行期无字符串查找。** 列引用解析为槽位下标；关联子查询通过作用域栈解析为
`ScalarExpr::Outer { levels_up, index }`，无需专门去关联即可正确执行。

**聚合语义对齐 PostgreSQL。** 分组时投影绑定在「聚合输出行」上：与 `GROUP BY` 表达式**结构相同**
的表达式改写为该键输出槽（故 `SELECT a+1 … GROUP BY a+1` 合法），其余裸列报出与 PG 相同的
`column "x" must appear in the GROUP BY clause …`（`42803`）。

**存储层为 MVCC 预留。** 行版本已带 `xmin`/`xmax`，`RowId` 即版本链所需形状；M1 用 `XID_NONE`，
M2 只需填字段，**不改布局、不重建索引**。

**约束在存储层校验。** `NOT NULL` 与唯一索引在 `Table::insert/update` 内检查，M2 的 WAL 回放同样受保护。

**响应形状照抄 PostgreSQL。** `Execute` 只回 `DataRow*` + `CommandComplete`，**不重复** `RowDescription`——
表头属于 `Describe`；多回一个 `T` 会让 `lib/pq` 直接以 `unexpected message during extended query execution:
"(T) RowDescription"` 中止语句。`Describe(statement)` 对 `INSERT/UPDATE/DELETE ... RETURNING` 也回 `RowDescription`
（不是 `NoData`），否则驱动拿到的行没有列名。`ParameterDescription`、`Bind` 的参数个数校验同理，
都在**驱动会检查它们的那一刻**给出与 PG 相同的答案。

**协议层遵循错误恢复规则。** 扩展协议中一旦出错，服务端**丢弃消息直到 `Sync`**（`sync_required` 标志）；
连接阶段的库不存在或凭据无效则在**任何 `ParameterStatus` 之前**发 `FATAL` 并关闭——这正是客户端区分
「连不上」与「语句失败」的依据。认证与库校验的顺序也照抄 PG：**先认证，再告知库是否存在**。

## 七、已支持 / 未支持

**已支持**：`CREATE/DROP ROLE|USER`（`PASSWORD`/`SUPERUSER`/`LOGIN`/`CREATEDB`/`CREATEROLE`/`IF [NOT] EXISTS`）、
`ALTER ROLE|USER`、SCRAM-SHA-256 认证与角色级权限、`CREATE/DROP DATABASE`（含 `IF [NOT] EXISTS`）、
`CREATE/DROP SCHEMA`、`CREATE TABLE`（`IF NOT EXISTS`、`DEFAULT`、`NOT NULL`、`PRIMARY KEY`、`UNIQUE`、表级约束）、
`CREATE TABLE AS SELECT`、`CREATE INDEX`（btree/hash、`UNIQUE`）、`CREATE [OR REPLACE] VIEW`、
`DROP TABLE`、`TRUNCATE`；`INSERT`（多行 `VALUES`、`INSERT … SELECT`、`DEFAULT`）、`UPDATE`、`DELETE` + `RETURNING`；
`SET`/`SHOW`（`SHOW ALL`、`SET TIME ZONE`、`SET x TO DEFAULT`，只读参数拒绝 `SET` 并报 `55P02`）；
会话参数按 PostgreSQL 的 13 个标准项经 `ParameterStatus` 上报（启动时与 `SET` 变更后），libpq/JDBC 系客户端可正常缓存；
**虚拟 `pg_catalog`**：`pg_database`/`pg_roles`/`pg_user`/`pg_namespace`/`pg_class`/`pg_attribute`/`pg_attrdef`/`pg_type`/
`pg_settings`/`pg_tables`/`pg_views`/`pg_indexes`/`pg_tablespace`/`pg_constraint`/`pg_am`/`pg_index`/
`pg_sequence`/`pg_sequences`
在查询时从活跃元数据即时物化（永不与真实状态漂移；`pg_constraint` 报主键与唯一约束，`conkey` 为 PG 的数组文本形式；
`pg_index.indkey` 为 PG 的空格向量形式；`pg_attrdef.adbin` 存放已渲染的默认值 SQL 文本，
`pg_get_expr(adbin, adrelid)` 返回该文本；`pg_class` 以 `relkind='S'` 列出序列，序列行随 `CREATE SEQUENCE`
接入后自动出现，`pg_sequences.last_value` 在首次 `nextval` 前为 `NULL`）；
`pg_matviews`/`pg_extension`/`pg_description`/`pg_depend`/`pg_inherits`/`pg_enum`/`pg_trigger`/
`pg_collation`/`pg_proc`/`pg_foreign_table`/`pg_foreign_server`/`pg_foreign_data_wrapper`/`pg_user_mapping`/
`pg_partitioned_table`/`pg_rewrite`/`pg_shdescription`/`pg_aggregate`/`pg_operator`/`pg_statistic`/`pg_policy`/
`pg_publication`/`pg_subscription` 以 PG 的真实列形存在但恒为空集——工具 JOIN 它们得到空结果而不是报错；
并支持 `pg_get_userbyid`/`pg_encoding_to_char`/`pg_table_is_visible`/`pg_get_expr`/`format_type`
（解码 `atttypmod`，`varchar(50)`/`numeric(10,2)` 正确回显）/`pg_get_viewdef`（名字、限定名与
`pg_class` 合成 OID 三种形式，渲染真实 `SELECT`）/`pg_get_indexdef`/`pg_get_serial_sequence`
（无 SERIAL 序列时返回 `NULL`）/`current_setting`/`pg_is_in_recovery`/`current_schema`/`version`/
`shobj_description`/`obj_description`/`col_description` 与 `pg_catalog.` 限定调用；
**`information_schema`**：`schemata`/`tables`/`columns`/`views`/`table_constraints`/`key_column_usage`
从活跃元数据物化（`data_type` 用标准名、`udt_name` 用 PG 内部名、`atthasdef`/`is_nullable` 等真实反映列定义），
`referential_constraints`/`check_constraints`/`routines`/`triggers`/`sequences` 为空集；
标准伪类型 `information_schema.character_data`/`sql_identifier`/`yes_or_no`/`cardinal_number`/`time_stamp`
与 PG 的对象标识符族 `oid`/`xid`/`cid`/`regclass`/`regproc`/`regnamespace`/`regrole`/`regtype` 等
（含 `pg_catalog.` 限定形式）映射到普通类型，`CAST(x AS oid)`、`::regclass` 可用；
`WHERE`、`ORDER BY`（`NULLS FIRST/LAST`、位置、别名）、`LIMIT/OFFSET`、`DISTINCT`/`DISTINCT ON`、
`GROUP BY`/`HAVING`（`count/sum/avg/min/max`，含 `DISTINCT` 与 `FILTER`）、`INNER/LEFT/RIGHT/FULL/CROSS JOIN`
（`ON`/`USING`/`NATURAL`）、派生表、非递归 CTE、`UNION/INTERSECT/EXCEPT [ALL]`、标量/`EXISTS`/`IN` 子查询（含关联）、
`EXPLAIN [ANALYZE]`；`WHERE pk = ?` 自动改写为 `Index Scan`。

**未支持 / 已知取舍**：

- **持久化是快照，不是 WAL**：后台任务把整个集群序列化到 `data/velocitysql.snapshot`
  （写入后尽快 + 周期兜底 + 退出前一次），重启最多丢失在飞行中的写入；没有事务原子性、没有崩溃点恢复（WAL 属 M2）。
  恢复是**尽力而为**：单个对象重建失败会被跳过而不是中止整体。引导超级用户的口令仍由 `--superuser-password` 决定，
  快照里它的口令会被忽略（避免旧口令「复活」）。
  快照带版本号，格式不兼容时**不会被覆盖**：启动会把它改名保留为 `data/velocitysql.snapshot.bak` 并以空库运行。
  `numeric` 与 JSON 以文本参与编码——`bincode` 不是自描述格式，而 `rust_decimal`/`serde_json` 的 `Deserialize`
  要走 `deserialize_any`，直接派生会让这两类值**写得进、读不回**；
  `a_snapshot_round_trips_every_value_the_engine_has` 逐个值类型钉住这件事。
- **事务无回滚语义**：`BEGIN/COMMIT/ROLLBACK` 仅做状态跟踪；`ROLLBACK TO SAVEPOINT` 显式报不支持。
- **无 TLS**：`SSLRequest` 以协议允许的 `'N'` 拒绝。因此口令虽经 SCRAM 保护（服务端始终看不到明文），
  整条连接仍是明文；`SCRAM-SHA-256-PLUS`（通道绑定）需要 TLS，属后续工作。
- **无关系级权限**：只有角色级属性（`SUPERUSER`/`CREATEDB`/`CREATEROLE`）；没有表所有权、`GRANT`/`REVOKE`、
  行级安全，也没有角色成员关系（`IN ROLE`/`INHERIT` 一律显式报不支持）。因此**任何能登录的角色都可以读写所有表**
  ——实测中 `reporter` 能 `DROP TABLE users` 正是这一点。
- **`DROP ROLE` 不检查依赖**：PG 会因「仍有对象依赖该角色」而拒绝，这里尚未实现依赖跟踪。
- **`pg_catalog` 是虚拟的**：上列目录关系在查询时从活跃元数据物化，工具连接期与浏览期的目录查询
  （`pg_database` 数据库列表、`pg_class`/`pg_attribute`/`pg_attrdef`/`pg_sequence`/`pg_tablespace`/
  `pg_constraint` 浏览、`format_type`/`pg_get_viewdef`/`pg_get_indexdef`/`pg_get_expr` 元数据渲染）已可用，
  Navicat/DBeaver 的表与视图浏览不受目录缺表阻塞。
  但 `psql` 的 `\d`/`\l` 还依赖少量函数（`array_to_string` 等）与 `regclass` 的**名字→OID 解析**
  （`'t'::regclass` 目前是 no-op，不查目录），仍可能报错；请继续用 `velocitysql-cli` 的
  `\l`/`\du`/`\dn`/`\dt`/`\d`。视图在 `pg_class` 中使用合成 OID（按名字散列），`pg_attribute` 暂不含
  视图列。
- **双引号在「不是列」时按字符串读**（MySQL 兼容，**有意偏离 PostgreSQL**）：`WHERE "type" = "allowRegister"`
  里若没有名为 `allowRegister` 的**未限定**列，该名字就读作字符串 `'allowRegister'`——按 MySQL 写法的客户端/工具
  常这么写引号。PG 只会把它当列名并报 `42703`。加限定名的（`"settings"."type"`）永远是列引用；
  未加引号的裸名也严格按 PG 处理，因此拼错的列名不会被静默当成字符串。
- **`SET` 接受未知参数名**（存起来即可），比 PostgreSQL 宽松——工具常设置引擎尚未建模的 GUC，
  拒绝它们比接受更伤兼容性；只读参数（`server_version`、`session_authorization` 等）仍显式拒绝。
  已知参数的值不改变执行行为（`datestyle`/`timezone` 只是记录并上报），`client_encoding` 仅接受 UTF-8。
- **扩展协议支持绑定参数与二进制结果格式**：`ParameterDescription` 报出参数个数（驱动据此校验实参），
  参数个数不符时在 `Bind` 阶段按 PG 的措辞拒绝（`08P01`，而不是拖到执行期报 `$n` 无值）；
  文本参数按「与之比较的那一侧的类型」解释，故 `int_col = $1` 传字符串 `'1'` 也能走索引。
  参数一律以文本读入，**含义由目标类型决定**：`{"a":1}` 进 `jsonb` 列就是 JSON 对象，而 `{1,2}`
  只有在目标确实是数组时才是数组（`int[]` 列、`id = ANY($1)`、`unnest($1)`）——
  在 `Bind` 期按「形状像 `{}`」猜数组会把 JSON 读成 `text[]`，正是这类报错的来源。
  结果可按列返回**二进制格式**（`bool`/`int2/4/8`/`float4/8`/`numeric`/`date`/`time`/`timestamp(tz)`/`interval`/
  `uuid`/`text`/`bytea`/`json(b)`）——`lib/pq` 对它认识的类型就要求二进制，回文本会被解成乱码；
  数组等尚无二进制编码，显式报不支持。**二进制参数格式**与 `Execute` 的行数上限同样显式拒绝。
- **`CREATE DATABASE ... TEMPLATE/ENCODING/OWNER`**、`CREATE ROLE ... VALID UNTIL/CONNECTION LIMIT` 等选项未实现，
  显式报不支持（不静默忽略）。
- 连接执行为物化内侧的嵌套循环（Hash Join 属 M5）；`UPDATE` 就地覆盖行（M2 改版本链）。
- 视图/CTE 在引用处内联执行，可能重复计算（M5 引入物化 CTE）。
- `varchar(n)` 的长度约束在写入时不做截断/校验。

## 八、里程碑

| 阶段 | 内容 | 状态 |
|------|------|------|
| M1 | 解析器 + 类型系统 + 内存表 + `SELECT/INSERT` | 完成 |
| M3 | PG Wire Protocol + `psql`/驱动可连 + 按库路由 + SCRAM 认证 | 主体完成（`pg_catalog`、TLS、绑定参数待补） |
| M2 | MVCC + WAL + 崩溃恢复 | 部分（异步快照持久化已落地：角色/库/schema/表/行/视图随重启恢复；MVCC 与 WAL 待做） |
| M4 | `ON CONFLICT`/`ALTER TABLE`/序列（`CREATE DATABASE`、`CREATE ROLE` 已提前完成） | 部分 |
| M5 | 成本模型与优化器、Hash Join、向量化、窗口函数 | 待做 |
| M6 | regress 对照测试、`pg_catalog` 物化（含 `pg_database`/`pg_roles`）、ORM 兼容、性能基线 | 部分（18 个目录关系已虚拟化并含序列/视图定义/索引 DDL 渲染，`psql \d` 所需的少量函数与 regclass 名字解析待补） |

## 许可

Apache-2.0
