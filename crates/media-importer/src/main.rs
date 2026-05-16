use color_eyre::Result;

fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt::init();

    println!("{}", media_importer::scaffold_message());

    Ok(())
}
