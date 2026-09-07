# TLSテスト用の証明書と鍵

`root.pem`、`chain.pem`、`end.key`は、[tokio-rustls](https://github.com/rustls/tokio-rustls) 0.26.4の公開crateに含まれる`tests/certs/`から変更せずに使用している。配布元とのバイト一致を確認した。

ローカルTLSテスト専用の公開fixtureであり、利用者の認証情報や本番用の鍵ではない。本番の証明書・鍵として使用しない。

配布元のMIT OR Apache-2.0からMITを選択し、[著作権・許諾表示](LICENSE-MIT)を同梱する。
