use iso22900_service::service::Iso22900Service;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    vci_service_launcher::start_service::<Iso22900Service, _, _>(std::env::args()).await?;
    Ok(())
}
