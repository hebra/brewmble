use async_trait::async_trait;

#[async_trait]
pub trait PackageManager: Send + Sync {
    fn name(&self) -> &str;
    fn is_available(&self) -> bool;
    async fn get_updates(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>>;
    async fn full_upgrade(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

pub mod apt;
pub mod brew;

pub fn get_package_manager() -> Box<dyn PackageManager> {
    let brew = brew::Brew;
    if brew.is_available() {
        return Box::new(brew);
    }
    Box::new(apt::Apt)
}
