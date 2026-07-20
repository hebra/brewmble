use async_trait::async_trait;
use std::io;
use std::process::{Command, Output};

#[async_trait]
pub trait PackageManager: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> String;
    fn is_available(&self) -> bool;
    async fn get_updates(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>>;
    async fn dry_run_upgrade(
        &self,
    ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>>;
    async fn full_upgrade(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    fn auto_clean(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    fn auto_remove(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output>;
}

pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        Command::new(program).args(args).output()
    }
}

pub mod apt;
pub mod brew;

pub fn get_package_manager(auto_clean: bool, auto_remove: bool) -> Box<dyn PackageManager> {
    #[cfg(target_os = "macos")]
    {
        Box::new(brew::Brew::new(auto_clean, auto_remove))
    }

    #[cfg(not(target_os = "macos"))]
    {
        let brew = brew::Brew::new(auto_clean, auto_remove);
        if brew.is_available() {
            return Box::new(brew);
        }
        Box::new(apt::Apt::new(auto_clean, auto_remove))
    }
}
