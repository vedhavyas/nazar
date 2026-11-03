//! The error model's transport classification: the class and the cause
//! chain, never the URL.

use std::time::Duration;

use qubo::QuboError;

#[tokio::test]
async fn a_refused_connection_is_classified_without_its_url()
-> Result<(), Box<dyn std::error::Error>> {
    let addr = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.local_addr()?
    };
    let Err(err) = reqwest::Client::new()
        .get(format!(
            "http://{addr}/sms/api/v1/sp/x/users/SECRET-UUID/auth/refresh"
        ))
        .send()
        .await
    else {
        return Err("the closed port answered".into());
    };
    let text = QuboError::from(err).to_string();
    assert!(
        text.starts_with("http error: connect"),
        "the class is named: {text}"
    );
    assert!(
        !text.contains("SECRET-UUID"),
        "the account id is scrubbed: {text}"
    );
    assert!(!text.contains("http://"), "the URL is scrubbed: {text}");
    Ok(())
}

#[tokio::test]
async fn a_dns_failure_is_classified_as_one() -> Result<(), Box<dyn std::error::Error>> {
    // The `.invalid` TLD is reserved to never resolve.
    let Err(err) = reqwest::Client::new()
        .get("http://nazar-does-not-exist.invalid/refresh")
        .send()
        .await
    else {
        return Err("the reserved host resolved".into());
    };
    let text = QuboError::from(err).to_string();
    assert!(
        text.starts_with("http error: dns"),
        "the class is named: {text}"
    );
    Ok(())
}

#[tokio::test]
async fn a_timeout_is_classified_as_one() -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        if let Ok((socket, _)) = listener.accept().await {
            // Held open and silent: the client must wait out its own bound.
            let _held = socket;
            std::future::pending::<()>().await;
        }
    });
    let Err(err) = reqwest::Client::new()
        .get(format!("http://{addr}/refresh"))
        .timeout(Duration::from_millis(100))
        .send()
        .await
    else {
        return Err("the silent listener answered".into());
    };
    let text = QuboError::from(err).to_string();
    assert!(
        text.starts_with("http error: timeout"),
        "the class is named: {text}"
    );
    Ok(())
}
