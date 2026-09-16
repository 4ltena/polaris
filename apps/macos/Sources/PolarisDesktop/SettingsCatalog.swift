import Foundation

enum SettingsCategory: String, CaseIterable, Identifiable {
    case general, appearance, models, accounts, permissions, integrations, workflow, memory, projects, maintenance
    var id: String { rawValue }
    var title: String {
        switch self {
        case .general: "一般"
        case .appearance: "外観"
        case .models: "モデル"
        case .accounts: "アカウント"
        case .permissions: "権限"
        case .integrations: "MCP・アプリ連携"
        case .workflow: "ワークフロー"
        case .memory: "長期記憶"
        case .projects: "プロジェクト・共同作業"
        case .maintenance: "保守・使用量"
        }
    }
    var symbol: String {
        switch self {
        case .general: "gearshape"
        case .appearance: "paintpalette"
        case .models: "cpu"
        case .accounts: "person.crop.circle"
        case .permissions: "lock.shield"
        case .integrations: "point.3.connected.trianglepath.dotted"
        case .workflow: "arrow.triangle.branch"
        case .memory: "books.vertical"
        case .projects: "folder"
        case .maintenance: "chart.bar"
        }
    }
}

enum SettingsDelivery {
    case existing, inProgress, planned
    var title: String {
        switch self {
        case .existing: "既存設定"
        case .inProgress: "準備中"
        case .planned: "実装予定"
        }
    }
}

enum SettingsControl {
    case information, theme, history, permission, model, localModel, project
}

struct SettingsItem: Identifiable {
    let id: String
    let category: SettingsCategory
    let title: String
    let detail: String
    let delivery: SettingsDelivery
    var control: SettingsControl = .information
    var keywords = ""
}

enum SettingsCatalog {
    // 実装状況の案内。実接続・認証・許可を示す値として使用しない。
    static let items: [SettingsItem] = [
        .init(id: "general.version", category: .general, title: "polarisについて", detail: "このアプリのバージョン。", delivery: .existing),
        .init(id: "general.setup", category: .general, title: "初回セットアップ", detail: "接続方法、作業フォルダ、許可の希望を設定します。既存のセットアップ画面を利用します。", delivery: .existing),
        .init(id: "general.documentation", category: .general, title: "ドキュメント", detail: "次回公開の直前に、日本語・英語・繁体中国語の資料を整理します。アプリの表示言語とは別の計画です。", delivery: .planned, keywords: "readme changelog docs 翻訳"),
        .init(id: "appearance.theme", category: .appearance, title: "テーマ", detail: "システムに合わせるか、ライト・ダークを選びます。", delivery: .existing, control: .theme),
        .init(id: "appearance.layout", category: .appearance, title: "パネルの配置", detail: "作業画面の境界をドラッグして調整できます。「配置を戻す」で初期配置に戻ります。", delivery: .existing, keywords: "サイドバー チャット 幅 レイアウト"),
        .init(id: "appearance.runs", category: .appearance, title: "実行内容の表示", detail: "実行の行から、保存された指示・ツール呼出し・結果を展開する機能を準備しています。", delivery: .inProgress, keywords: "ログ run 詳細"),
        .init(id: "models.main", category: .models, title: "主モデル", detail: "接続先、モデル、推論の強さを既存のモデル設定で選びます。", delivery: .existing, control: .model, keywords: "reasoning effort GPT Codex OpenAI ChatGPT API"),
        .init(id: "models.local", category: .models, title: "ローカルモデルと役割", detail: "Ollama・LM Studioのモデルと、役割ごとの割当を確認します。検出と接続は明示操作で行います。", delivery: .existing, control: .localModel),
        .init(id: "models.routing", category: .models, title: "処理に応じたモデルの振り分け", detail: "定型処理はモデルなし、限定的な判断は軽量モデル、複雑な判断は主モデルへ振り分ける予定です。", delivery: .planned, keywords: "Luna Astra Sol 自動"),
        .init(id: "accounts.current", category: .accounts, title: "ログインと認証", detail: "現在の認証状態は、この画面へまだ接続されていません。保存された接続方法だけでログイン済みとは判定しません。", delivery: .inProgress, keywords: "ChatGPT OAuth API key キー"),
        .init(id: "accounts.multiple", category: .accounts, title: "複数アカウント", detail: "登録、保存・復元、認証更新と、実行中に使うアカウントの固定を準備しています。", delivery: .inProgress),
        .init(id: "accounts.quota", category: .accounts, title: "利用枠と切替", detail: "各アカウントの利用枠の取得・表示と切替を準備しています。未取得の枠を残量ゼロとは表示しません。", delivery: .inProgress),
        .init(id: "permissions.preference", category: .permissions, title: "自動許可の希望", detail: "プロジェクト内で希望する操作範囲です。実行時の承認やmacOSの許可を変更する設定ではありません。", delivery: .existing, control: .permission),
        .init(id: "permissions.external", category: .permissions, title: "外部ツールの操作範囲", detail: "接続先と操作内容に応じた承認を扱います。この画面での権限管理は準備中です。", delivery: .inProgress, keywords: "computer use ブラウザ MCP"),
        .init(id: "integrations.stdio", category: .integrations, title: "ローカルMCPサーバ", detail: "stdioで起動するサーバの登録と、利用するツール・リソース・プロンプトの選択を準備しています。", delivery: .inProgress),
        .init(id: "integrations.http", category: .integrations, title: "リモートMCPサーバ", detail: "Streamable HTTP接続の登録、接続状態、認証と利用範囲の表示を準備しています。", delivery: .inProgress),
        .init(id: "integrations.blender", category: .integrations, title: "Blender", detail: "MCP共通経路の完成後に、対応版と操作範囲を確定します。", delivery: .planned),
        .init(id: "integrations.resolve", category: .integrations, title: "DaVinci Resolve", detail: "映像制作アプリとの連携を予定しています。接続方法と対応版は未確定です。", delivery: .planned),
        .init(id: "integrations.unity", category: .integrations, title: "Unity", detail: "MCP共通経路の後に、エディタとの連携を予定しています。", delivery: .planned),
        .init(id: "integrations.unreal", category: .integrations, title: "Unreal Engine", detail: "MCP共通経路の後に、エディタとの連携を予定しています。", delivery: .planned, keywords: "UE"),
        .init(id: "workflow.stages", category: .workflow, title: "作業段階とコンテキスト", detail: "現在の段階に必要な情報だけを渡す仕組みを実装済みです。設定画面への接続は準備中です。", delivery: .inProgress, keywords: "state 計画 実装 レビュー 検証"),
        .init(id: "workflow.goals", category: .workflow, title: "継続する目標と追加指示", detail: "目標の実行中も指示を受け付け、追加指示を優先する予定です。明示停止後は自動で再開しません。", delivery: .planned, keywords: "goal 自律"),
        .init(id: "workflow.delegation", category: .workflow, title: "委任と復旧", detail: "子の進捗をイベントで受け取り、終了・成果・重複を確認して必要な場合だけ復旧する予定です。", delivery: .planned, keywords: "subagent Luna Fusion"),
        .init(id: "workflow.intake", category: .workflow, title: "初動応答と入力整理", detail: "Lunaによる初動応答と入力整理を予定しています。創作など、要約で意図が失われる入力は原文を保持します。", delivery: .planned),
        .init(id: "workflow.templates", category: .workflow, title: "定型開発と検証", detail: "環境構築や決まった検査を再利用し、繰り返し生成する処理を減らす予定です。", delivery: .planned, keywords: "Next.js git_setting nextjs_setting"),
        .init(id: "workflow.refactor", category: .workflow, title: "余剰枠での改善提案", detail: "余った利用枠でリファクタリング案を作り、別ファイルへ保存する予定です。提案を実装済みの変更とは扱いません。", delivery: .planned),
        .init(id: "memory.history", category: .memory, title: "直接渡す会話の範囲", detail: "strict10では直近10ターンを直接渡します。過去の原文も出典・再取得用として保存します。", delivery: .existing, control: .history, keywords: "履歴 圧縮"),
        .init(id: "memory.structured", category: .memory, title: "固有名詞と現在の決定", detail: "正式名・別名の辞書、決定・撤回の状態と原文の参照を管理します。設定画面への接続は準備中です。", delivery: .inProgress, keywords: "structured10 状態表 訂正"),
        .init(id: "memory.search", category: .memory, title: "検索と埋め込み", detail: "関連する過去の文脈を取得します。参照枠、埋め込み資源と検索状態の表示は準備中です。", delivery: .inProgress, keywords: "vector ベクトル e5 日本語 768"),
        .init(id: "memory.skills", category: .memory, title: "必要なスキルの取得", detail: "関連するスキルの候補だけを取得します。選択状態の表示は準備中です。", delivery: .inProgress, keywords: "BM25 skill"),
        .init(id: "projects.folder", category: .projects, title: "作業フォルダ", detail: "現在の作業場所を確認・選択します。", delivery: .existing, control: .project),
        .init(id: "projects.home", category: .projects, title: "プロジェクトを開いていないときの閲覧", detail: "ホームフォルダを既定として表示する機能を準備しています。作業対象や権限とは分けて扱います。", delivery: .inProgress, keywords: "~ ファイルツリー home"),
        .init(id: "projects.sharing", category: .projects, title: "共有と変更履歴", detail: "共有リンクとPolaris Projectsの独立したGit履歴を予定しています。衝突時には双方の変更を保持します。", delivery: .planned, keywords: "同期 offline reconnect"),
        .init(id: "projects.linear", category: .projects, title: "Linear", detail: "タスク連携を予定しています。Linearの業務状態とPolarisの実行状態を分けて扱います。", delivery: .planned),
        .init(id: "projects.organize", category: .projects, title: "フォルダの整理案", detail: "フォルダの類似度から再編案を作る機能を予定しています。", delivery: .planned),
        .init(id: "projects.device-checks", category: .projects, title: "実機確認の依頼", detail: "人が確認する操作と期待結果を整理して出力する機能を予定しています。", delivery: .planned),
        .init(id: "maintenance.cleanup", category: .maintenance, title: "生成物の定期整理", detail: "不要なビルド生成物の整理を予定しています。使用中・現行版・共有の成果物は保護します。", delivery: .planned, keywords: "ストレージ 容量 削除"),
        .init(id: "maintenance.cost", category: .maintenance, title: "使用量と費用", detail: "入力・出力・キャッシュ・補助処理・再試行を含めた費用の表示を予定しています。未計測の値は表示しません。", delivery: .planned, keywords: "tokens token トークン reasoning API"),
        .init(id: "maintenance.compression", category: .maintenance, title: "出力とログの圧縮", detail: "全機能の統合比較後に、品質を保って出力と総費用を減らす方式を評価します。根拠となる原文と実行結果は保持します。", delivery: .planned, keywords: "Headroom キャッシュ"),
        .init(id: "maintenance.power", category: .maintenance, title: "端末負荷", detail: "CPU・メモリ・取得可能な電力指標の計測と改善を予定しています。", delivery: .planned),
        .init(id: "maintenance.web", category: .maintenance, title: "Web本文の取得", detail: "通常のHTTP取得と、必要時だけ描画する経路を比較する予定です。Moliの採用は効果測定後に判断します。", delivery: .planned, keywords: "検索 ブラウザ Moli"),
    ]

    static func matching(_ query: String, category: SettingsCategory? = nil) -> [SettingsItem] {
        func normalize(_ text: String) -> String {
            text.folding(options: [.caseInsensitive, .diacriticInsensitive, .widthInsensitive], locale: Locale(identifier: "ja_JP"))
        }
        let words = normalize(query).split(whereSeparator: { $0.isWhitespace })
        return items.filter { item in
            let text = normalize([item.title, item.detail, item.category.title, item.keywords].joined(separator: " "))
            return (category == nil || item.category == category) && words.allSatisfy { text.contains($0) }
        }
    }
}
