# 海康 HCUSBSDK ThermalV2 — UVC 2.0 XU 控制协议逆向笔记

来源：`libHCUSBSDK.so`（Android arm64，dynsym 完整）静态逆向 + `/tmp/uhikcamera.apk`
（com.hcusbsdk）dex 字段名交叉验证。所有偏移、事务序列均从反汇编提取；
标注「推测」的条目未在真机上验证。

## 1. 总览

- 设备为 UVC 摄像头（ThermalV2 机型，协议版本字符串 `"2.0"`）。
- 所有配置/测温命令走 **UVC Extension Unit（XU）控制传输**：
  - XU **Unit ID = 10（0x0a）**，挂在 **interface 0**，因此 **`wIndex` 恒为 `0x0a00`**。
  - `wValue = selector << 8`，selector 即 XU control selector。
- selector 分配：

  | selector | 含义 |
  |---|---|
  | 1 | system 命令组（wValue `0x0100`） |
  | 2 | image 命令组（wValue `0x0200`） |
  | 3 | thermal/测温 命令组（wValue `0x0300`） |
  | 4 | 协议版本探测 |
  | 5 | 「当前命令」保持寄存器 |
  | 6 | 最近一次错误码 |
  | 7 | audio 组、8 = ptz 组、9 = vca 组（本机未用） |

- 底层就是标准 `libusb_control_transfer`：
  `bmRequestType / bRequest / wValue / wIndex / data / wLength / timeout`。
  SDK 内部 trans 结构：`+0 u32 方向(0=IN,1=OUT)`、`+8 data`、`+0x10 wLength`、
  `+0x18 bmRequestType`、`+0x19 bRequest`、`+0x1c wValue`、`+0x1e wIndex`、`+0x20 timeout(ms)`。
  IN 时 bmRequestType 强制置 0x80，OUT 强制清 0x80。SDK 侧实际用到的
  bmRequestType：GET 用 `0xA1`，SET 用 `0x21`（class, interface）。
- bRequest 用 UVC 标准码：`SET_CUR=0x01`、`GET_CUR=0x81`、`GET_LEN=0x85`。

## 2. 初始化 / 打开序列（免 login）

`CUsbDeviceThermalV2::Open` 只做版本探测，**不发 login**（`USB_Login` →
`CUsbDeviceACS::Login` 仅用于门禁 ACS 设备，与热成像无关）。

`DetectDeviceVersion`：

1. `GET_LEN(0x85)`，`wValue=0x0400`，`wIndex=0x0a00`，读 4 字节长度（应返回 4）。
2. `GET_CUR(0x81)`，`wValue=0x0400`，读 4 字节，与 `"2.0\0"` memcmp。
3. 不匹配时走兼容路径：`SET_CUR sel3`（wValue=0x0300）发 2 字节 `{0x00, 0x05}`
   （含义未确定，推测为降级/切换协议版本）。

### selector 5 — 命令保持寄存器

每个命令执行前 `SwitchCommand → HoldCommand`：SDK 缓存 `(wValue组, subFunction)`，
仅当与上次不同才发：

```
SET_CUR(0x01) wValue=0x0500 wIndex=0x0a00 len=2  payload={组号, subFunction}
```

`组号` = 数据 selector（1/2/3…），`subFunction` = 命令子功能号（见 §5 表）。
设备此后把该组的数据通道「定位」到这条命令。实测：不 streaming 时该 SET 无响应
（设备 NAK/超时），streaming 时正常 —— 发命令前需要先开好流。

### selector 6 — 错误码

`GET_CUR(0x81) wValue=0x0600` 读 1 字节 = 最近一条命令的错误码（0 = OK；
实测在非法状态下返回 0x07）。

## 3. 通用事务模式

### 3.1 简单 GET（例：2030 GET_THERMOMETRY_BASIC_PARAM）

```
1. SET_CUR sel5 {03, sub}            # 命令切换（有缓存，相同命令可省）
2. GET_LEN(0x85) wValue=0x0300 len=4 → u32 LE = 数据长度 L   # GetDataLen，上限 0x200
3. GET_CUR(0x81) wValue=0x0300 len=L → payload
```

响应 payload 的 **第 0 字节不回读**（疑为 subfunc 回显），字段从 `payload[1]` 开始。

### 3.2 简单 SET（例：2031 SET_THERMOMETRY_BASIC_PARAM）

```
1. SET_CUR sel5 {03, sub}
2. GET_LEN sel3 → L（先读回长度，决定缓冲区）
3. SET_CUR(0x01) wValue=0x0300 len=L payload=结构体
```

payload[0] = `byChannelID`。**TB-4117-3/S 实测必须为 1**（热成像；填 2 时
Phase A 后 GET_LEN 回显请求长度随后 pipe error）。

### 3.3 DoubleGet（2046 JPEGPIC / 2047 ROI_SEARCH / 2054 CALIBRATION_FILE / 2051）

带请求结构体 + 大响应的命令分两阶段（`DoubleGetDeviceConfig`）。**已在
TB-4117-3/S 真机验证（2046/2047），与反汇编推测有两处出入**：

```
Phase A: SET_CUR sel3 payload=请求结构(含通道号/参数)
         GET_LEN sel3 -> 5
         GET_CUR sel3 读 5 字节: buf[0]=0x01, buf[1..5) = u32 LE 结果总长度 N
Phase B: 直接 GET_CUR 读结果 —— 切勿再次 SET_CUR（会重置事务，后续 GET 卡 pipe）
```

- **真机实测**：Phase B 每次控制读 **wLength 不得超过 512**，否则 pipe error；
  超过 64KB 则报 Invalid parameter。数据分片返回，每片以 5 字节片头
  `{0x02, u32 LE 序号}` 开始（小响应也是单片带片头），host 剥掉片头按序拼到
  N 字节为止。N 为净 payload 长度（不含片头）。
- （反汇编推测、真机未用到：`WriteDataStrategy` 大 SET 每片 `0x1fd` + 5 字节片头。）

## 4. 命令号（cmdId）→ selector / subFunction 表

cmdId 是 SDK API `USB_GetDeviceConfig/SetDeviceConfig` 的命令号；
组号即数据 selector（wValue 高字节）；sub 即 selector 5 写入的子功能号。
GET/SET 共享同一 sub。

| cmd | 组 | sub | 结构体 | 说明 |
|---|---|---|---|---|
| 2011 | 1 | 1 | USB_SYSTEM_DEVICE_INFO | 设备信息（GET） |
| 2013 | 1 | 3 | — | 重启等系统控制（推测） |
| 2014/2015 | 1 | 4 | USB_SYSTEM_HARDWARE_SERVER | 硬件服务状态 |
| 2016/2017 | 1 | 5 | USB_SYSTEM_LOCALTIME | 设备时间 |
| 2000 | 1 | 6 | UPDATE_FIRMWARE_* | 固件升级 |
| 2024/2050 | 1 | 7 | USB_SYSTEM_DIAGNOSED_DATA | 诊断数据（DoubleGet） |
| 4002 | 1 | 10 | USB_SYSTEM_ENCRYPT_STATUS | |
| 4003/4004 | 1 | 11 | USB_SYSTEM_INDICATORLIGHT | 指示灯 |
| 4051 | 1 | 14 | USB_SYSTEM_DEVICE_CAPABILITIES | 能力集 |
| 2018/2019 | 2 | 1 | USB_IMAGE_BRIGHTNESS | 亮度（u32） |
| 2020/2021 | 2 | 2 | USB_IMAGE_CONTRAST | 对比度（u32） |
| 2026/2027 | 2 | 5 | USB_IMAGE_ENHANCEMENT | 图像增强 |
| 2028/2029 | 2 | 6 | USB_IMAGE_VIDEO_ADJUST | 图像调节 |
| **2030/2031** | 3 | 1 | USB_THERMOMETRY_BASIC_PARAM | 测温基本参数 |
| **2032/2033** | 3 | 2 | USB_THERMOMETRY_MODE | 测温模式 |
| **2034/2035** | 3 | 3 | USB_THERMOMETRY_REGIONS | 测温区域 |
| 2036 | 3 | 4 | USB_THERMAL_ALG_VERSION | 算法版本（GET） |
| 2038/2039 | 3 | 5 | USB_THERMAL_STREAM_PARAM | 热成像流参数 |
| 2040/2041 | 3 | 6 | USB_TEMPERATURE_CORRECT | 温度修正 |
| 2042/2043 | 3 | 7 | USB_BLACK_BODY | 黑体 |
| 2044/2045 | 3 | 8 | USB_BODYTEMP_COMPENSATION | 体温补偿 |
| 2046 | 3 | 9 | USB_JPEGPIC_WITH_APPENDDATA | 抓图（DoubleGet） |
| **2047** | 3 | 10 | USB_ROI_MAX_TEMPERATURE_SEARCH(_RESULT) | ROI 点/区域测温（DoubleGet） |
| 2048/2049 | 3 | 11 | USB_P2P_PARAM | 点对点测温参数 |
| 2051 | 3 | 12 | USB_DOUBLE_LIGHTS_CORRECT(_RESULT) | 双光校准（DoubleGet） |
| 2052/2053 | 3 | 13 | USB_DOUBLE_LIGHTS_CORRECT_POINTS_CTRL | |
| 2054/2055 | 3 | 14 | USB_THERMOMETRY_CALIBRATION_FILE | 标定文件（DoubleGet） |
| 2056/2057 | 3 | 15 | USB_THERMOMETRY_EXPERT_REGIONS | 专家测温区域 |
| 2058/2059 | 3 | 16 | USB_THERMOMETRY_EXPERT_CORRECTION_PARAM | 专家修正 |
| 2060 | 3 | 17 | — | |
| 2061/2062 | 3 | 18 | USB_THERMOMETRY_RISE_SETTINGS | 温升设置 |
| 2063/2064 | 3 | 19 | USB_ENVIROTEMPERATURE_CORRECT | 环境温度修正 |
| 40xx | 7 | * | USB_AUDIO_* | 音频 |
| 4037–4041 | 8 | 1–5 | — | PTZ |
| 41xx | 9 | * | USB_VCA_* | 智能分析 |

1xxx（门禁/卡）与 4501（DEVICE_VERSION）走 ISUSB 私有协议，不在 UVC XU 上。

## 5. 关键结构体线上格式（全 LE；GET 响应从 payload[1] 起，SET payload[0]=byChannelID）

### 5.1 USB_THERMOMETRY_BASIC_PARAM（2030/2031，sub 1）— 47 字节

（**TB-4117-3/S 实测 GET 返回 32 字节**，疑固件版本差异，尾部字段被截短；
实测发射率 0x62=98（即 0.98）在偏移 16、距离 100 在偏移 21，与下表吻合。）

字段名来自 dex `com.hcusbsdk.Interface.USB_THERMOMETRY_BASIC_PARAM` +
`ParseRecvData<USB_THERMOMETRY_BASIC_PARAM>` 日志格式串，顺序完全确认：

| 偏移 | 类型 | 字段 |
|---|---|---|
| 0 | u8 | byChannelID（SET 时；GET 响应此字节不回读） |
| 1 | u8 | byEnabled 测温使能 |
| 2 | u8 | byDisplayMaxTemperatureEnabled |
| 3 | u8 | byDisplayMinTemperatureEnabled |
| 4 | u8 | byDisplayAverageTemperatureEnabled |
| 5 | u8 | byTemperatureUnit（0=℃ 1=℉ 推测） |
| 6 | u8 | byTemperatureRange（测温档位） |
| 7 | u8 | byCalibrationCoefficientEnabled |
| 8 | u32 | dwCalibrationCoefficient |
| 12 | u32 | dwExternalOpticsWindowCorrection |
| 16 | u32 | dwEmissivity 发射率（×100，如 95 = 0.95） |
| 20 | u8 | byDistanceUnit |
| 21 | u32 | dwDistance（单位 cm，推测） |
| 25 | u8 | byReflectiveEnable |
| 26 | u32 | dwReflectiveTemperature（×100，推测） |
| 30 | u8 | byThermomrtryInfoDisplayPosition |
| 31 | u8 | byThermometryStreamOverlay |
| 32 | u32 | dwAlert 预警温度（×100，推测） |
| 36 | u32 | dwAlarm 报警温度（×100，推测） |
| 40 | u32 | dwExternalOpticsTransmit |
| 44 | u8 | byDisplayCenTempEnabled |
| 45 | u8 | byBackcolorEnabled |
| 46 | u8 | byShowAlarmColorEnabled |

### 5.2 USB_THERMOMETRY_MODE（2032/2033，sub 2）— 3 字节

`[1]=byThermometryMode（0=普通 1=专家 推测）`，`[2]=byThermometryROIEnabled`。

### 5.3 USB_THERMOMETRY_REGIONS（2034/2035，sub 3）— 3 + 10×18 = 183 字节

- `[2] = byRegionNum`
- `[3 + 18*i]` 起 10 个区域，每区 18 字节（dex `THERMAL_REGION`）：
  `{u8 byRegionID, u8 byRegionEnabled, u32 dwRegionX, u32 dwRegionY, u32 dwRegionWidth, u32 dwRegionHeight}`

### 5.4 USB_THERMAL_ALG_VERSION（2036，sub 4，仅 GET）— 64 字节

`payload[0..64)` 原样为算法名字符串 szThermometryAlgName。

### 5.5 USB_ROI_MAX_TEMPERATURE_SEARCH（2047，sub 10，DoubleGet）

请求 payload（`ConvertData<...>` 输出，共 234 = 0xea 字节）：

| 偏移 | 类型 | 字段 |
|---|---|---|
| 0 | u8 | byChannelID |
| 1 | u16 | wMillisecond |
| 3 | u8 | bySecond |
| 4 | u8 | byMinute |
| 5 | u8 | byHour |
| 6 | u8 | byDay |
| 7 | u8 | byMonth |
| 8 | u16 | wYear |
| 10 | u8 | byJpegPicEnabled（是否回传 JPEG 图） |
| 11 | u8 | byMaxTemperatureOverlay |
| 12 | u8 | byRegionsOverlay |
| 13 | u8 | byROIRegionNum（0–10） |
| 14 + 22*i | — | 10 个 ROI 区域，每区 22 字节：`{u8 byROIRegionID, u8 byROIRegionEnabled, u32 dwROIRegionX, u32 dwROIRegionY, u32 dwROIRegionWidth, u32 dwROIRegionHeight, u32 dwDistance}` |

坐标/尺寸为**像素 u32**（不是浮点比例）。**真机实测：坐标空间是 480×640
（显示分辨率 240×320 的 2 倍）**；1×1 的区域可用，可用于单像素测温。

响应（Phase B 读回的净 payload，**已真机验证**，与下表一致）：

| 偏移 | 类型 | 字段 |
|---|---|---|
| 0 | u8 | 0x01（疑为回显/标志） |
| 1 | u32 | dwMaxP2PTemperature 全局最高温 |
| 5 | u32 | dwVisibleP2PMaxTemperaturePointX |
| 9 | u32 | dwVisibleP2PMaxTemperaturePointY |
| 13 | u32 | dwThermalP2PMaxTemperaturePointX |
| 17 | u32 | dwThermalP2PMaxTemperaturePointY |
| 21 | u8 | byROIRegionNum（本机恒为 0x0a=10，疑为容量） |
| 22 | u32 | dwJpegPicLen（后续 JPEG 数据长度，可为 0） |
| 26 + 21*i | — | 区域结果，每区 21 字节：`{u8 byROIRegionID, u32 dwMaxROIRegionTemperature, u32 VisX, u32 VisY, u32 dwThermalROIRegionMaxTemperaturePointX, u32 ThermalY}` |
| 236 (0xec) | — | JPEG 数据，dwJpegPicLen 字节 |

**温度换算（真机实锤）**：`dwXxxTemperature` 为 u32 定点，**×10 °C**
（如 367 = 36.7 ℃），与 OSD 显示值一致（手掌全覆盖画面时 36.7°C 两边完全相同）。

**实测注意**：
- 结果坐标同样在 480×640 空间。
- 未请求的区域槽位内容是上次查询的残留（id 字节被清零，其余照旧），
  解析时只读请求数量的条目。
- 2047 的搜索对小热点有稀释（吸顶灯 40.5°C 用 1×1 区域只读到 30.9°C，
  疑算法在降采样/平滑后的图上跑）。**精确逐像素温度请用 2046**（见 §5.5a）。

### 5.5a USB_JPEGPIC_WITH_APPENDDATA（2046，组 3 sub 9，DoubleGet）— 真机验证

请求 payload 13 字节：`[0] u8 byChannelID (=1)`，`[1..10]` 时间戳（同 2047
头部，可全 0 只填 wYear），`[10..13]` 全 0。

响应净 payload（总长 N 由 Phase A 给出，本机约 89 KB）：

| 偏移 | 类型 | 字段（实测值） |
|---|---|---|
| 0 | u8 | 0x01 |
| 1 | u32 | JPEG 长度 jl（约 12 KB） |
| 5 | u32 | 温度图宽度 = 120 |
| 9 | u32 | 温度图高度 = 160 |
| 13 | u32 | 温度数据字节数 = w×h×4 = 76800 |
| 17..27 | — | 保留（本机为 300, 260, 0） |
| 27 | — | JPEG 图像（120×160，无 OSD），jl 字节 |
| 27+jl | — | **120×160 f32 LE 温度矩阵，单位 °C**，行优先 |

温度矩阵与 OSD 完全一致（吸顶灯处 40.59 vs OSD 40.5°C），是逐像素测温的
正确途径。矩阵坐标 ×2 即显示坐标（240×320）。

### 5.6 USB_SYSTEM_DEVICE_INFO（2011，组 1 sub 1）— 484 (0x1e4) 字节

线上即 C 结构体原样（ParseRecvData 为整体 memcpy），8 个定长字符串 + 1 个 u32：
`[0x00:0x40)` str，`[0x40:0x80)` str，`[0x80:0xc0)` str，`[0xc0:0x100)` str，
`[0x100]` u32，`[0x104:0x144)` str，`[0x144:0x184)` str，`[0x184:0x1a4)` str(32)，
`[0x1a4:0x1e4)` str。
字段名候选（dex `USB_SYSTEM_DEVICE_INFO`）：bySerialNumber / byDeviceType /
byDeviceID / byHardwareVersion / byFirmwareVersion / bySecondHardwareVersion /
byEncoderVersion / byModuleID / byProtocolVersion。**各槽位与名称的对应关系未验证**，
u32 槽位疑为协议/版本号。

### 5.7 USB_THERMOMETRY_EXPERT_REGIONS（2056/2057，sub 15）— 3 + 21×139 = 2922 字节

- `[2] = byRegionNum`，`[3] [4]` 为首个区域的头部两字节（见下）。
- 区域记录从 `[3 + 139*i]` 起，每条 139 (0x8b) 字节，最多 21 条：

  | 记录内偏移 | 类型 | 字段（dex `THERMAL_EXPERT_REGIONS`，逐名对应为最佳推测） |
  |---|---|---|
  | 0 | u8 | byRegionID |
  | 1 | u8 | byEnabled |
  | 2 | 32B | szName |
  | 0x22 | u32 | dwEmissivity |
  | 0x26 | u32 | dwDistance |
  | 0x2a | u8 | byReflectiveEnable |
  | 0x2b | u32 | dwReflectiveTemperature |
  | 0x2f | u8 | byShowAlarmColorEnabled |
  | 0x30 | u8 | byRule |
  | 0x31 | u8 | byType |
  | 0x32 | u32 | dwAlert |
  | 0x36 | u32 | dwAlarm |
  | 0x3a | u8 | byPointNum |
  | 0x3b | u32 | （保留/未命名） |
  | 0x3f | u32 | （保留/未命名） |
  | 0x43 | 8×8B | struRegionCoordinate[8]：`{u32 dwPointX, u32 dwPointY}` |
  | 0x83 | 4B | （尾部保留，记录跨距 0x8b，含 4B 填充） |

### 5.8 其余简单结构（字段名来自 dex，线上即字段顺序排布，u32 LE）

- USB_TEMPERATURE_CORRECT（2040/41，sub 6）：byEnabled, byCorrectEnabled,
  byStreamOverlay, dwTemperature, dwCorrectTemperature, dwEmissivity, dwDistance,
  dwCentrePointX, dwCentrePointY
- USB_BLACK_BODY（2042/43，sub 7）：byEnabled, dwTemperature, dwEmissivity,
  dwDistance, dwCentrePointX, dwCentrePointY
- USB_P2P_PARAM（2048/49，sub 11）：byJpegPicEnabled
- USB_IMAGE_BRIGHTNESS / CONTRAST（2018/2020，组 2）：单个 u32
- USB_THERMAL_STREAM_PARAM（2038/39，sub 5）：byVideoCodingType

## 6. 实测备注

- 发任何命令前确认视频流已启动，否则 selector 5 的 SET_CUR 会超时。
- 命令出错后读 selector 6 拿错误码。
- GetDataLen 返回上限 0x200；真实长度以 GET_LEN 读到的 u32 为准。
- 所有多字节整数均小端。
