use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use llm::InferenceStats;
use rand::{rngs::StdRng, SeedableRng};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{
    cmp::min,
    collections::HashMap,
    convert::Infallible,
    env,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
    time::Instant,
};

#[derive(Parser)]
struct Cli {
    /// The path to the directory containing the model configurations.
    /// If not specified, the default directory will be used.
    #[clap(short, long)]
    configs: Option<PathBuf>,

    /// Whether to use memory mapping when loading the model.
    #[clap(short, long)]
    no_mmap: bool,

    /// The model architecture to test. If not specified, all architectures will be tested.
    architecture: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse command line arguments
    let args = Cli::parse();
    let specific_model = args.architecture.clone();

    // Initialize directories
    let cwd = env::current_dir()?;
    let configs_dir = args
        .configs
        .unwrap_or_else(|| cwd.join("binaries/llm-test/configs"));
    let download_dir = cwd.join(".tests/models");
    fs::create_dir_all(&download_dir)?;
    let results_dir = cwd.join(".tests/results");
    fs::create_dir_all(&results_dir)?;

    // Load configurations
    let mut configs = HashMap::new();
    for entry in fs::read_dir(configs_dir)? {
        let path = entry?.path();
        if path.is_file() && path.extension().map_or(false, |ext| ext == "json") {
            let file_name = path.file_stem().unwrap().to_str().unwrap().to_string();
            let test_case: TestCase = serde_json::from_str(&fs::read_to_string(&path)?)?;
            configs.insert(file_name, test_case);
        }
    }
    let model_config = ModelConfig {
        mmap: !args.no_mmap,
    };

    // Test models
    if let Some(specific_architecture) = specific_model {
        if let Some(test_case) = configs.get(&specific_architecture) {
            println!("Key: {}, Config: {:?}", specific_architecture, test_case);
            test_model(&model_config, test_case, &download_dir, &results_dir).await?;
        } else {
            println!("No config found for {}", specific_architecture);
        }
    } else {
        for (key, test_case) in &configs {
            println!("Key: {}, Config: {:?}", key, test_case);
            test_model(&model_config, test_case, &download_dir, &results_dir).await?;
        }
    }

    println!("All tests passed!");
    Ok(())
}

struct ModelConfig {
    mmap: bool,
}

#[derive(Deserialize, Debug)]
struct TestCase {
    url: String,
    filename: PathBuf,
    architecture: String,
}

#[derive(Serialize)]
pub struct Report {
    pub could_loaded: bool,
    pub inference_stats: Option<InferenceStats>,
    pub error: Option<String>,
    pub output: String,
}

async fn test_model(
    config: &ModelConfig,
    test_case: &TestCase,
    download_dir: &Path,
    results_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("Testing architecture: `{}` ...", test_case.architecture);

    let local_path = if test_case.filename.is_file() {
        // If this filename points towards a valid file, use it
        test_case.filename.clone()
    } else {
        // Otherwise, use the download dir
        download_dir.join(&test_case.filename)
    };

    // Download the model
    download_file(&test_case.url, &local_path).await?;

    let start_time = Instant::now();

    // Load the model
    let architecture = llm::ModelArchitecture::from_str(&test_case.architecture)?;
    let model_result = llm::load_dynamic(
        Some(architecture),
        &local_path,
        llm::TokenizerSource::Embedded,
        llm::ModelParameters {
            prefer_mmap: config.mmap,
            ..Default::default()
        },
        llm::load_progress_callback_stdout,
    );

    let model = match model_result {
        Ok(m) => m,
        Err(err) => {
            // Create a report with could_loaded set to false
            let report = Report {
                could_loaded: false,
                inference_stats: None,
                error: Some(format!("Failed to load model: {}", err)),
                output: String::new(),
            };

            // Serialize the report to a JSON string
            let json_report = serde_json::to_string(&report)?;
            let report_path = results_dir.join(format!("{}.json", test_case.architecture));

            // Write the JSON report to a file
            fs::write(report_path, json_report)?;

            // Optionally, you can return early or decide how to proceed
            return Err(Box::new(err));
        }
    };

    println!(
        "Model fully loaded! Elapsed: {}ms",
        start_time.elapsed().as_millis()
    );

    // Run the model
    let mut session = model.start_session(Default::default());

    let prompt = "write a story about a lama riding a crab:";
    let mut rng: StdRng = SeedableRng::seed_from_u64(42);
    let mut output = String::new();

    println!("Running inference...");
    let res = session.infer::<Infallible>(
        model.as_ref(),
        &mut rng,
        &llm::InferenceRequest {
            prompt: prompt.into(),
            parameters: &llm::InferenceParameters {
                n_threads: 2,
                n_batch: 1,
                ..Default::default()
            },
            play_back_previous_tokens: false,
            maximum_token_count: Some(10),
        },
        &mut Default::default(),
        |r| match r {
            llm::InferenceResponse::PromptToken(t) | llm::InferenceResponse::InferredToken(t) => {
                output += &t;
                Ok(llm::InferenceFeedback::Continue)
            }
            _ => Ok(llm::InferenceFeedback::Continue),
        },
    );
    println!("Inference done!");

    // Process the results
    let (inference_results, error) = match res {
        Ok(result) => (Some(result), None),
        Err(err) => (None, Some(err)),
    };

    // Save the results
    let report = Report {
        could_loaded: true,
        inference_stats: inference_results,
        error: error.map(|e| format!("{:?}", e)),
        output,
    };

    // Serialize the report to a JSON string
    let json_report = serde_json::to_string(&report)?;
    let report_path = results_dir.join(format!("{}.json", test_case.architecture));

    // Write the JSON report to a file
    fs::write(report_path, json_report)?;

    // Optionally, panic if there was an error
    if let Some(err) = &report.error {
        panic!("Error: {}", err);
    }

    println!(
        "Successfully tested architecture `{}`!",
        test_case.architecture
    );

    Ok(())
}

async fn download_file(url: &str, local_path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    if Path::new(local_path).exists() {
        println!("Model already exists at {}", local_path.to_str().unwrap());
        return Ok(());
    }

    let client = Client::new();

    let mut res = client.get(url).send().await?;
    let total_size = res.content_length().ok_or("Failed to get content length")?;

    let pb = ProgressBar::new(total_size);
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({eta})")
        .progress_chars("#>-"));

    let mut file = File::create(local_path)?;
    let mut downloaded: u64 = 0;

    while let Some(chunk) = res.chunk().await? {
        file.write_all(&chunk)?;
        let new = min(downloaded + (chunk.len() as u64), total_size);
        downloaded = new;
        pb.set_position(new);
    }

    pb.finish_with_message("Download complete");

    Ok(())
}
