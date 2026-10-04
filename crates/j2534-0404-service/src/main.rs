use j2534_0404_service::service::J2534Service;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    vci_service_launcher::start_service::<J2534Service, _, _>(std::env::args()).await?;
    Ok(())
}
