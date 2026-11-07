//! GUI・TUI の起動確認と文字起こしで共通の AWS 設定を使う。
use crate::config::TranscribeConfig;
use anyhow::Result;
use aws_credential_types::Credentials;

pub async fn load(config: &TranscribeConfig) -> Result<aws_config::SdkConfig> {
    config.aws.validate()?;
    let auth = &config.aws;
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(config.region.clone()));
    if !auth.access_key_id.is_empty() {
        loader = loader.credentials_provider(Credentials::new(
            auth.access_key_id.clone(),
            auth.secret_access_key.clone(),
            (!auth.session_token.is_empty()).then(|| auth.session_token.clone()),
            None,
            "dcr-config",
        ));
    } else if !auth.profile.is_empty() {
        // 明示したプロファイルは環境変数の別アカウント認証より優先する。
        let provider = aws_config::profile::ProfileFileCredentialsProvider::builder()
            .profile_name(&auth.profile)
            .build();
        loader = loader.profile_name(&auth.profile).credentials_provider(provider);
    }
    Ok(loader.load().await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_credential_types::provider::ProvideCredentials;

    #[tokio::test]
    async fn explicit_keys_and_region_reach_sdk_without_default_login() {
        let config: TranscribeConfig = toml::from_str(
            "[aws]\naccess_key_id = 'test-access'\nsecret_access_key = 'test-secret'\nsession_token = 'test-session'",
        ).unwrap();
        let sdk = load(&config).await.unwrap();
        assert_eq!(sdk.region().unwrap().as_ref(), "ap-northeast-1");
        let creds = sdk.credentials_provider().unwrap().provide_credentials().await.unwrap();
        assert_eq!(creds.access_key_id(), "test-access");
        assert_eq!(creds.secret_access_key(), "test-secret");
        assert_eq!(creds.session_token(), Some("test-session"));
    }

    #[test]
    fn rejects_partial_or_ambiguous_auth_and_redacts_debug() {
        for input in [
            "access_key_id = 'a'", "secret_access_key = 's'", "session_token = 't'",
            "profile = 'default'\naccess_key_id = 'a'\nsecret_access_key = 's'",
            "profile = ' '",
        ] {
            let auth: crate::config::AwsConfig = toml::from_str(input).unwrap();
            assert!(auth.validate().is_err());
        }
        let auth: crate::config::AwsConfig = toml::from_str(
            "access_key_id = 'example-access'\nsecret_access_key = 'example-secret'\nsession_token = 'example-token'",
        ).unwrap();
        let debug = format!("{auth:?}");
        for value in ["example-access", "example-secret", "example-token"] {
            assert!(!debug.contains(value));
        }
        assert!(crate::config::AwsConfig::default().validate().is_ok());
    }
}
