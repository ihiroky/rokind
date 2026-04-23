# Rokind Reminder

Google Calendar の予定を常駐監視し、開始 5 分前と取得時点で進行中の予定を全ディスプレイへ always-on-top のリマインドで表示する Tauri v2 アプリです。

設定画面を閉じてもアプリはタスクトレイに常駐し、必要なタイミングでリマインド専用ウィンドウを前面表示します。

## できること

- Google Calendar の予定を 30 秒おきに監視
- 開始 5 分前と開始時刻以降の進行中予定を重複防止つきで通知
- 全ディスプレイの上部中央付近にリマインドウィンドウを表示
- Meeting URL を Google Meet / `conferenceData` / 場所 / 説明文の順で抽出
- 直近 12 時間以内の予定を一覧表示
- 一覧から Meeting URL をクリップボードにコピー
- リマインド画面から Meeting URL をそのまま開く
- メインウィンドウを閉じるとタスクトレイへ格納
- タスクトレイから設定画面の再表示とアプリ終了
- OS ログイン時の自動起動を切り替え
- 多重起動を防止し、再起動時は既存ウィンドウを前面表示

## 動作の前提

- 対象カレンダーは Google Calendar の `primary` カレンダーです
- 対象は日時つきイベントです。終日予定は表示対象になりません
- OAuth 設定はプロジェクト直下の `.env` からビルド時に埋め込みます
- アクセストークンと取得済みイベントはアプリデータ配下の `state.json` に保存されます
- `GOOGLE_CLIENT_ID` / `GOOGLE_CLIENT_SECRET` 自体は保存されません

## セットアップ

### 1. Google Cloud で OAuth Client を作成

Google Cloud Console で OAuth Client を作成します。

- Application type: `Desktop app`
- Scope: `https://www.googleapis.com/auth/calendar.readonly`

### 2. `.env` を作成

`.env.example` を参考に、プロジェクト直下へ `.env` を作成します。

```dotenv
GOOGLE_CLIENT_ID=your-client-id.apps.googleusercontent.com
GOOGLE_CLIENT_SECRET=your-client-secret
```

- `GOOGLE_CLIENT_ID` は必須です
- `GOOGLE_CLIENT_SECRET` は Google 側の設定によって必要な場合だけ使われます
- `.env` の内容はビルド時に埋め込まれるため、変更後は再ビルドが必要です

### 3. 開発環境を用意

少なくとも以下が必要です。

- Node.js
- `pnpm`
- Rust toolchain
- Tauri v2 の実行に必要な OS 依存ライブラリ

Linux では追加パッケージが必要です。

Debian / Ubuntu 系:

```bash
sudo apt install pkg-config libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libsoup-3.0-dev
sudo apt install libwebkit2gtk-4.1-dev
```

環境によっては `libwebkit2gtk-4.0-dev` が必要です。Fedora 系では `webkit2gtk4.1-devel` や `gtk3-devel` 相当を入れてください。

### 4. 起動

```bash
pnpm install
pnpm run tauri dev
```

## 使い方

1. アプリを起動する
2. `Google で接続` を押してブラウザで認証する
3. 接続後、Google Calendar の予定が一覧に表示される
4. メインウィンドウを閉じるとタスクトレイ常駐に切り替わる
5. 予定の開始 5 分前と、取得時点ですでに開始している予定を含めて各ディスプレイへリマインドが表示される

補足:

- 認証中に設定画面を閉じても、バックエンド側で OAuth 完了待ちは継続します
- `予定を再取得` で手動同期できます
- 自動起動は設定画面のトグルから切り替えられます
- すでに起動中の状態でもう一度アプリを開くと、新しいプロセスは終了し、既存の設定画面が前面に表示されます

## ビルド

```bash
pnpm run build
pnpm run tauri build
```

## 実装メモ

- OAuth は loopback redirect (`http://127.0.0.1:{port}/oauth/callback`) を使います
- `refresh_token` を取得できた場合は access token を自動更新します
- 認証が失効した場合は接続状態をリセットし、再接続を促す動きです
- Meeting URL は `hangoutLink` を優先し、なければ `conferenceData`、さらに `location` / `description` 内の URL を見ます
- メインウィンドウのクローズは終了ではなく非表示です。完全終了はトレイメニューの `終了` から行います
