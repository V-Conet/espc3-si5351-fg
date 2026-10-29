# ESP32-C3 + Si5351 信号发生器

[English](README.md)

## 简介
基于 ESP32-C3（Rust / ESP-IDF）的 Si5351A 三路时钟信号发生器。默认通过 **BLE** 控制；设备同时开启一个**无密码热点 `Si5351-FG`**，浏览器打开 http://192.168.23.1/ 即可通过网页控制。也可以下发路由器 Wi-Fi 凭据，让设备以 AP+STA 方式连接路由器。

<table>
  <tr>
    <td align="center" valign="middle" width="200">
      <img src="docs/esp32c3.webp" alt="ESP32C3-SuperMini"/>
        <a href="https://www.nologo.tech/product/esp32/esp32c3/esp32c3supermini/esp32C3SuperMini.html"><small>ESP32C3-SuperMini 4MB</small></a>
    </td>
    <td align="center" valign="middle" width="200">
      <img src="docs/si5351.webp" alt="SI5351"/>
        <a href="https://e.tb.cn/h.j4qThQqLoWK24KT?tk=BE5mTzXoozT"><small>淘宝 SI5351时钟信号发生器模块</small></a>
    </td>
  </tr>
</table>


### 效果图

<table>
  <tr>
    <td align="center" valign="middle" rowspan="2">
      <img src="docs/real.webp" alt="ESP32C3-SuperMini" width="400"/><br/>
      <small> 测试设备为 DS4T252 </small>
    </td>
    <td align="center" valign="middle">
      <img src="docs/DSTSS12.bmp" width="300"/>
    </td>
  </tr>
  <tr>
    <td align="center" valign="middle">
      <img src="docs/DSTSS13.bmp" width="300"/>
    </td>
  </tr>
</table>




## 功能

- 三路独立输出（CLK0 / CLK1 / CLK2），频率范围 **7.813 kHz – 160 MHz**，每路可单独开关。
- 单路扫频：可设置起始/终止频率、步进、驻留时间和是否循环，支持 BLE / HTTP 启动与停止。
- 输出驱动强度可选 2 / 4 / 6 / 8 mA。
- BLE 默认开启，设备名 `Si5351-FG`，无需按键或串口即可控制。
- 默认开启无密码 SoftAP：SSID `Si5351-FG`，IP **192.168.23.1**，HTTP 页面直接可用。
- 可通过 BLE 或 AP 页面下发路由器 SSID / 密码；设备以 AP+STA 方式同时连接路由器，热点保持可用。
- Wi-Fi 凭据及设置可保存到 NVS，上电自动恢复。
- BLE 与 Wi-Fi 可以同时工作：AP/STA 运行时 BLE 仍然保持可用。

## 硬件接线

ESP32-C3 与 Si5351 之间只需要 I2C 和电源：

| ESP32-C3 | Si5351 模块 | 说明 |
| --- | --- | --- |
| `3V3` | `VIN` | 3.3 V |
| `GND` | `GND` | 共地，必须连接 |
| `GPIO1` | `SDA` | I2C 数据 |
| `GPIO0` | `SCL` | I2C 时钟 |
|  | `CLK0` / `CLK1` / `CLK2` |  |

> 当前代码使用 **SDA = GPIO1、SCL = GPIO0**。GPIO0/1 同时也是 ADC / XTAL_32K 引脚，但可以通过 GPIO 矩阵正常用作 I2C；它们同样避开了 Strapping 引脚（GPIO2/8/9）和 USB-Serial-JTAG 引脚（GPIO18/19）。如果开发板在 GPIO0/1 上接了 32.768 kHz 晶振，请改用 GPIO4/5。
>
> 
>
> 如需改引脚，修改 `src/main.rs` 中的 `peripherals.pins.gpio1/gpio0`，并同步更新 `src/config.rs` 的 `I2C_SDA_GPIO` / `I2C_SCL_GPIO`（用于日志）。GPIO4/5 是常用的替代选择。

## 构建与烧录

### 环境要求

- Rust nightly（项目使用 `build-std`，见 `rust-toolchain.toml`）
- `espflash`（项目已配置为 cargo runner）：`cargo install espflash`
- 首次构建时 `embuild` 会自动准备 ESP-IDF v6.1（`ESP_IDF_TOOLS_INSTALL_DIR=global`，安装到 `~/.espressif`）
- 建议 4 MB Flash 的 ESP32-C3 开发板（见下方分区说明）

### 命令

```bash
cargo build --release
cargo run --release    # 构建、烧录后运行
```

或者手动烧录：

```bash
espflash flash --monitor \
  target/riscv32imc-esp-espidf/release/espc3-si5351-fg
```

### 配置说明

`sdkconfig.defaults` 已启用：

- NimBLE（BLE only，关闭 Bluedroid / Central / Observer / Broadcaster，只保留 Peripheral + GATT Server）
- Wi-Fi 与 BLE 软件共存
- 4 MB Flash + 1.5 MB 应用分区
- HTTP 服务器请求头上限 1024 字节

如果开发板只有 2 MB Flash，需要把 `CONFIG_ESPTOOLPY_FLASHSIZE_4MB=y` 改为对应容量，并换用更小的分区表；此时固件可能放不下，建议使用 4 MB 及以上。

## BLE 控制

设备广播名：`Si5351-FG`（网页按此前缀过滤）。

### GATT 服务

Service UUID：`53510000-0001-4a5b-8c9d-e0f123456789`

| 特征 | UUID | 属性 | 值格式 |
| --- | --- | --- | --- |
| Command | `53510000-0002-4a5b-8c9d-e0f123456789` | Write / WriteNoRsp | ASCII 命令行，`\n` 结尾 |
| Response | `53510000-0003-4a5b-8c9d-e0f123456789` | Read / Indicate | 最近一条回复 |
| CLK0 状态 | `53510000-0004-4a5b-8c9d-e0f123456789` | Read | `freq_hz,enabled,drive_ma` |
| CLK1 状态 | `53510000-0005-4a5b-8c9d-e0f123456789` | Read | 同上 |
| CLK2 状态 | `53510000-0006-4a5b-8c9d-e0f123456789` | Read | 同上 |
| Wi-Fi 状态 | `53510000-0007-4a5b-8c9d-e0f123456789` | Read | `state,ip,reason`（state：0 未配置、1 连接中、2 已连接、3 失败、4 热点模式；reason 为 ESP-IDF 断开原因码，0 表示无） |
| Info | `53510000-0008-4a5b-8c9d-e0f123456789` | Read | `version,uptime_seconds,si_ok`（`si_ok`：1 表示 Si5351 通信正常） |
| Sweep 状态 | `53510000-0009-4a5b-8c9d-e0f123456789` | Read | `running,channel,current_hz` |

Command 特征可能被按 MTU 分片写入；固件会缓存并在收到换行符后执行。单条命令不超过 1024 字节。

### 命令协议

| 命令 | 说明 | 回复 |
| --- | --- | --- |
| `PING` | 连通性测试 | `OK PONG` |
| `SET <ch> <freq_hz> [<on> <ma>]` | 设置频率；可选同时设置开关和驱动 | `OK S<ch> <freq_hz>` |
| `ON <ch>` / `OFF <ch>` | 打开 / 关闭输出 | `OK O<ch> <0\|1>` |
| `DRIVE <ch> <ma>` | 设置驱动电流（2/4/6/8） | `OK D<ch> <ma>` |
| `SAVE` | 保存当前通道设置到 NVS | `OK SAVE` |
| `RESET` | 恢复默认通道设置 | `OK RESET` |
| `WIFI <ssid_hex> <pass_hex\|->` | 下发 Wi-Fi；SSID/密码为 UTF-8 的十六进制，空密码用 `-` | `OK WIFI`，随后 `WIFI...` → `OK <ip>` 或 `ERR WIFI` |
| `FORGET`（或 `WIFI CLEAR`） | 删除已保存的路由器凭据，回到默认热点模式 | `OK FORGET` |
| `SWEEP <ch> <start> <end> <step> <dwell_ms> [<loop>]` | 启动扫频；频率/步进单位为 Hz，`loop=1` 循环 | `OK SWEEP` 或 `ERR SWEEP` |
| `SWEEP STOP` | 停止扫频 | `OK STOP` |
| `STATUS` | 刷新状态 | `OK`（具体状态请读 CLKx / Wi-Fi 特征） |

`<ch>` 为 0/1/2。示例：

```
SET 0 10000000 1 8
ON 1
DRIVE 2 4
WIFI 6d794170 70617373776f7264
SAVE
```

## Wi-Fi 与 HTTP

设备启动后会立即开启一个**无密码热点**：

- SSID：`Si5351-FG`
- 地址：**http://192.168.23.1/**
- HTTP 服务监听 80 端口，AP 客户端通过 DHCP 获得 `192.168.23.x`

网页里的 Wi-Fi 表单可以填写路由器的 SSID / 密码：

- 设备会切换到 **AP+STA** 模式，AP 保持可用，同时 STA 连接路由器（仅 2.4 GHz，ESP32-C3 不支持 5 GHz）。
- 连接成功后，设备的 STA IP 可从 Wi-Fi 状态特征、页面或串口日志读取；AP 的 `192.168.23.1` 仍然可访问。
- 如果 STA 连接失败，AP 仍然可用，可以在 `192.168.23.1` 页面查看原因并重试。
- 页面上的“忘记 Wi-Fi”/`FORGET` 会删除 NVS 中的路由器凭据并回到 AP-only 模式，重启后也不会自动连接旧网络。
- 切换/重连路由器时，热点可能会短暂重启；AP 与 STA 共用射频，AP 信道可能随 STA 改变。页面断开后重新连接热点并刷新即可。
- AP 固定使用 `192.168.23.0/24`。如果路由器 LAN 也使用该网段，请修改 `src/config.rs` 中的 `AP_IP`（以及页面提示），避免路由冲突。

| 方法与路径 | 说明 |
| --- | --- |
| `GET /` | 控制页面 |
| `GET /api/status` | 设备状态 JSON |
| `POST /api/channel?ch=0&freq=10000000&on=1&drive=8` | 修改通道；`freq`、`on`、`drive` 可省略 |
| `POST /api/wifi?ssid=...&pass=...` | 下发 Wi-Fi（参数需 URL 编码） |
| `POST /api/wifi/forget` | 删除已保存的路由器凭据，回到热点模式 |
| `POST /api/sweep?ch=0&start=1000000&end=10000000&step=100000&dwell=100&loop=0` | 启动扫频 |
| `POST /api/sweep/stop` | 停止扫频 |
| `POST /api/save` | 保存通道设置 |
| `POST /api/reset` | 恢复默认通道设置 |

`/api/status` 返回示例：

```json
{
  "fw": "v0.1.0",
  "uptime": 123,
  "ble": true,
  "si": true,
  "ch": [
    {"f": 10000000, "on": 1, "d": 8},
    {"f": 1000000, "on": 0, "d": 8},
    {"f": 1000000, "on": 0, "d": 8}
  ],
  "wifi": {"state": 2, "ip": "192.168.1.23", "ssid": "mywifi", "msg": "", "reason": 0},
  "sweep": {"running": false, "ch": 0, "current": 0}
}
```

## 网页控制器

文件：[`web/index.html`](web/index.html)

- **BLE 模式**：用 Chrome / Edge 打开页面，点击“连接蓝牙”，选择 `Si5351-FG`。
- **HTTP 模式**：连接设备热点 `Si5351-FG`（无密码）后打开 `http://192.168.23.1/`；或设备连接路由器后打开 STA IP。页面会自动使用 HTTP API。
- 页面无外部依赖，自动跟随系统亮/暗色，移动端可用。
- 界面支持中文 / English：按浏览器首选语言自动选择，右上角 `EN` / `中文` 可手动切换（会记住选择）；也可用 `?lang=en` 或 `?lang=zh` 强制语言。

Web Bluetooth 需要安全上下文：

- 推荐在本机启动静态服务器后访问 `http://localhost:8000/web/index.html`（localhost 被视为安全上下文）：
  ```bash
  python3 -m http.server 8000
  ```
  
- 也可以把页面放到任意 HTTPS 静态托管。

- iOS Safari 不支持 Web Bluetooth；请使用 Android Chrome 或桌面 Chrome/Edge。

## 频率范围与限制

- 输出范围：**7.813 kHz – 160 MHz**（板载 25 MHz 晶振）。
- 输出为 3.3 V CMOS 方波，经 SMA 输出；驱动强度可选 2/4/6/8 mA。
- CLK0 使用 PLL A，CLK1 使用 PLL B，CLK2 优先使用 PLL A，必要时切到 PLL B。
- 高于 112.5 MHz 的输出需要把 PLL 调到对应 VCO（4/6 分频的整数模式）。如果三路同时要求互相冲突的高频 VCO，设备会拒绝该组合并返回 `ERR SYNTH`；此时可把其中一路移到其他频率，或只用两路高频输出。
- 低于约 1 MHz 时自动使用 R 分频（1–128）。
- 频率精度主要取决于 25 MHz 晶振本身（模块通常 ±20–30 ppm）；分频计算误差 < 1 ppm。

## 默认设置

| 通道 | 频率 | 状态 | 驱动 |
| --- | --- | --- | --- |
| CLK0 | 10 MHz | 开启 | 8 mA |
| CLK1 | 1 MHz | 关闭 | 8 mA |
| CLK2 | 1 MHz | 关闭 | 8 mA |

## 扫频

网页“扫频”区域或 BLE/HTTP 命令都可以启动单路扫频：

- 目标通道：CLK0 / CLK1 / CLK2
- 起始/终止频率、步进：单位可选 Hz / kHz / MHz
- 驻留时间：20–60000 ms（每一步从开始写寄存器计时，I2C 重配时间包含在内）
- 循环：扫到终点后自动回到起点继续
- 最大 20000 个频点

扫频开始时会自动启用目标通道；扫频结束或停止后，输出停留在最后一个频率并保持启用。扫频运行期间网页会禁用三路手动控制，先点“停止”再修改。实际步进周期 = 驻留时间与 I2C 重配置时间中的较大者，因此高频点/慢速 I2C 下会略慢于设定值。若某个频点与其它通道的组合无法合成，扫频会停止，具体原因见串口日志。

## 安全说明

BLE（无配对/PIN）、HTTP（无登录）以及默认的 `Si5351-FG` **无密码热点**都没有认证。任何在热点覆盖范围内的设备都可以连接并控制输出。请只在可信环境中使用；如需部署在不可信环境，请自行增加 WiFi 密码、HTTP 认证或 BLE 配对。

## 常见问题

- **串口出现 I2C 错误 / 频率不变**：检查 3V3→VIN、GND、GPIO1→SDA、GPIO0→SCL；确认模块电源指示灯亮、地址为 0x60。固件使用 100 kHz I2C（`src/config.rs` 的 `I2C_BAUD_HZ`）并启用了内部上拉；如果模块未供电，SDA/SCL 没有被上拉，会表现为 `ESP_ERR_TIMEOUT` 而不是 NACK。
- **没看到 BLE**：即使 Si5351 未连接，BLE/Wi-Fi 也会启动；串口应出现 `NimBLE started, advertising as Si5351-FG`。如果仍然没有，检查 `sdkconfig.defaults` 是否被重新生成（`sdkconfig` 缓存）并 `cargo clean` 后重编。
- **Si5351 热插拔/后接**：固件每 5 秒重试一次 I2C，接好后无需重启即可自动配置。
- **手机搜不到 BLE**：确认设备已启动并打印 `BLE advertising as Si5351-FG`；同一时间只允许 1 个 BLE 连接（`CONFIG_BT_NIMBLE_MAX_CONNECTIONS=1`）。
- **STA Wi-Fi 连不上**：AP 热点仍然可用，继续访问 `http://192.168.23.1/` 即可。ESP32-C3 只支持 2.4 GHz；WPA2 / WPA3 / WPA2-WPA3 混合网络均受支持。网页会显示 ESP-IDF 的断开原因码，常见值：
  - `201`：找不到该 SSID（确认存在 2.4 GHz 网络、名称没有多余空格）
  - `202` / `15` / `204`：认证或握手失败，通常是密码错误
  - `211`：AP 的安全模式低于阈值（开放/WEP 网络）
  - `212` / `200`：信号太弱
  - `205`：连接失败，可重试
  密码通过十六进制传输，网页会自动编码；开放网络请留空密码。
- **固件超过分区大小**：默认使用 1.5 MB 应用分区。若链接报 overflow，可改用自定义 2 MB 分区表或 `opt-level = "z"`。
- **浏览器提示 Web Bluetooth 不可用**：使用 Chrome/Edge、确认页面处于安全上下文（localhost 或 HTTPS），不要使用 Firefox / iOS Safari。

## Thanks to

[@adafruit/adafruit_si5351_library](https://github.com/adafruit/adafruit_si5351_library)

## License

MIT
