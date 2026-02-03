use async_trait::async_trait;

#[async_trait]
pub trait PackageManager: Send + Sync {
    fn name(&self) -> &str;
    fn is_available(&self) -> bool;
    async fn get_updates(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>>;
    async fn full_upgrade(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

pub mod apt;

pub fn get_package_manager() -> Box<dyn PackageManager> {
    Box::new(apt::Apt)
}
