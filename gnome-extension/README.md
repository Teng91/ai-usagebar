# AI Usage Bar — GNOME Shell 擴充套件

這是 [`ai-usagebar`](../README.md) 的客製化 GNOME Shell 前端，會在頂端面板同時建立三個獨立指示器：

- **GPT**（底層 vendor ID：`openai`）
- **OpenRouter**（底層 vendor ID：`openrouter`）
- **Gemini**（透過 Google Antigravity，底層 vendor ID：`antigravity`）

每個指示器都有自己的服務圖示與原生下拉選單。擴充套件會呼叫同一套
`ai-usagebar` CLI 取得資料，再使用 GNOME `St` 元件繪製額度、進度條與重設時間；它不會把
Waybar tooltip 原樣塞進 GNOME 面板。

這個 fork 的 GNOME 前端只顯示上述三項服務。底層 CLI 與 TUI 仍保留上游專案支援的其他
AI 服務商。

![GNOME 頂端面板同時顯示 GPT 的 5 小時與每週額度、OpenRouter 剩餘額度，以及 Gemini 的兩個每週額度池](../screenshots/gnome-usagebar.png)

## 顯示內容

- **GPT**：顯示 5 小時與每週用量、進度條及重設倒數。
- **OpenRouter**：直接顯示 `{or_balance}` 回傳的剩餘額度。
- **Gemini**：顯示 Antigravity 提供的兩個每週額度池；`G` 代表 Gemini，`C` 代表
  Claude & GPT OSS。
- 點擊任一指示器會開啟該服務的原生下拉選單。
- 下拉選單可立即更新資料、開啟 `ai-usagebar-tui`，或進入擴充套件設定。
- 面板與設定介面使用繁體中文。

三個指示器使用圖片圖示，不需要 Nerd Font。進度條使用等寬字型，確保填滿與未使用區段
能夠整齊對齊。

## 系統需求

- GNOME Shell **45–50**（ESM extension）。
- 系統中必須能找到 `ai-usagebar`；擴充套件依序檢查偏好設定中的自訂路徑、`PATH`，以及
  `~/.cargo/bin/ai-usagebar`。
- 若要從選單開啟 TUI，需要安裝 `ai-usagebar-tui`，以及 `kgx`、`gnome-terminal` 或
  `xterm` 其中之一。
- 安裝方式請參考[主 README](../README.md)。

## 各服務設定

| 顯示名稱 | 資料來源 | 需要的設定 |
|---|---|---|
| GPT | `~/.codex/auth.json` | 安裝 Codex CLI 後執行 `codex login`。憑證會自動重新整理。 |
| OpenRouter | API Key | 設定 `OPENROUTER_API_KEY`，或在 `config.toml` 的 `[openrouter]` 填入 `api_key`。也可由偏好設定開啟 TUI 設定。 |
| Gemini | 本機 Antigravity 服務 | 開啟 Antigravity App、IDE 或互動式 `agy` 工作階段；不需要另外登入。 |

## 開發版安裝

取得這個 fork 與客製化分支：

```bash
git clone https://github.com/Teng91/ai-usagebar.git
cd ai-usagebar
git switch joy/gnome-ai-usagebar
cd gnome-extension
```

安裝擴充套件：

```bash
./install.sh

# X11：Alt+F2，輸入 r 後按 Enter
# Wayland：登出後重新登入
gnome-extensions enable ai-usagebar@akitaonrails.github.io
```

手動安裝方式：

```bash
UUID=ai-usagebar@akitaonrails.github.io
DEST="$HOME/.local/share/gnome-shell/extensions/$UUID"
glib-compile-schemas schemas/
mkdir -p "$DEST" && cp -r . "$DEST"
```

目前保留上游 UUID `ai-usagebar@akitaonrails.github.io`，讓既有安裝可以直接升級。請勿同時
安裝上游 GNOME 擴充套件與此 fork，兩者會使用相同 UUID。

## 偏好設定

```bash
gnome-extensions prefs ai-usagebar@akitaonrails.github.io
```

| 設定 | 預設值 | 說明 |
|---|---:|---|
| 顯示 5 小時額度 | 開啟 | 控制 GPT 的 5 小時額度。 |
| 顯示每週額度 | 開啟 | 控制 GPT 的每週額度，以及 Gemini 的兩個每週額度池。 |
| 顯示額外使用量 | 關閉 | 資料存在時顯示額外費用進度條。 |
| 顯示百分比／數值 | 開啟 | 在進度條旁顯示百分比或數值。 |
| 顯示進度條 | 開啟 | 關閉後只顯示數值。 |
| 每個進度條寬度 | 8 | 可設定為 4–20 個字元。 |
| 更新間隔 | 30 秒 | 可設定為 5–3600 秒。 |
| 執行檔路徑 | 自動 | 留空時從 `PATH` 或 `~/.cargo/bin` 尋找。 |
| 區域 | `right` | 三個指示器共同放在 `left`、`center` 或 `right`。 |
| 區域內排序 | 0 | GPT 從此位置開始，OpenRouter 與 Gemini 依序排列。 |

「AI 服務商」頁面只列出 GPT、OpenRouter 與 Gemini，可檢查目前設定狀態並啟動相對應的
登入或設定流程。服務種類是固定的，因此沒有 vendor selector。

## 資料取得與顯示方式

擴充套件會非同步執行以下三種命令，因此 CLI 請求不會阻塞 GNOME Shell：

```text
ai-usagebar --vendor openai --format <format>
ai-usagebar --vendor openrouter --format <format>
ai-usagebar --vendor antigravity --format <format>
```

實際使用的 format 為：

```text
{plan};;{session_pct};;{session_reset};;{weekly_pct};;{weekly_reset};;{sonnet_pct};;{sonnet_reset};;{extra_pct};;{extra_spent};;{extra_limit};;{scoped_model};;{scoped_pct};;{scoped_reset};;{session_elapsed};;{weekly_elapsed};;{scoped_elapsed};;{vendor_short};;{extra_model};;{extra_reset};;{extra_elapsed};;{session_model};;{weekly_model};;{or_balance};;__aiub_end__
```

程式解析 CLI 的 Waybar JSON（`{text, tooltip, class}`），再從 `text` 取出上述欄位。最後的
`__aiub_end__` 是保護 elapsed 欄位不受 stale `⏸` 後綴影響的 sentinel。

進度條沿用 `ai-usagebar` 的 One Dark 預設配色：90% 以上為紅色、75–89% 為橘色、
50–74% 為黃色，其餘為綠色。當資料同時提供有效的 reset 與 elapsed 值時，進度條會加入
藍色 `│` pace marker。

## Gemini 額度池

Antigravity 會提供 Gemini 與 Claude & GPT OSS 兩個獨立額度池。頂端面板以 `G` 和 `C`
分別顯示兩個每週用量；兩者各自保留用量百分比、重設時間與 pace marker。關閉「顯示每週
額度」會同時隱藏這兩段資訊。

資料來自目前正在執行的 Antigravity App、IDE 或互動式 `agy` 工作階段。如果這些程式都已
關閉，擴充套件會先顯示 CLI 快取；快取過期後則顯示錯誤狀態。

## 商標

GPT、OpenAI、OpenRouter、Gemini、Google Antigravity，以及相關標誌均為其各自權利人所有；
本專案使用名稱與圖示僅為識別對應服務，不代表獲得其背書或合作。
