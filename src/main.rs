use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fmt::{self, Write},
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    process::{self, Output, Stdio},
};

use anyhow::Context as _;
use clap::Parser;
use serde::{de::IgnoredAny, Deserialize};
use termtree::Tree;

/// Print the module structure of a Terraform project
#[derive(Parser, Debug)]
pub struct Args {
    /// Load variable values from the given file, in addition to the default files terraform.tfvars
    /// and *.auto.tfvars. Use this option more than once to include more than one variables file.
    #[arg(long)]
    var_file: Vec<String>,
    /// 'foo=bar'. Set a value for one of the input variables in the root module of the configuration. Use
    /// this option more than once to set more than one variable.
    #[arg(long)]
    var: Vec<String>,
    /// Limit the number of concurrent operations.
    #[arg(long, default_value = "10")]
    parallelism: Option<u32>,

    /// The path to terraform project.
    #[arg(long, default_value = ".")]
    path: PathBuf,
}

#[derive(Deserialize)]
struct Show<'a> {
    #[serde(borrow = "'a")]
    configuration: Configuration<'a>,
}

#[derive(Deserialize)]
struct Configuration<'a> {
    #[serde(borrow = "'a")]
    root_module: Module<'a>,
}

#[derive(Deserialize)]
struct Module<'a> {
    #[serde(borrow = "'a")]
    module_calls: Option<HashMap<&'a str, ModuleCall<'a>>>,
}

impl<'a> Module<'a> {
    fn into_trees<'b>(
        self,
        base: &'b Path,
        parent: PathBuf,
    ) -> impl Iterator<Item = Tree<TreeNode<'a>>> + 'b
    where
        'a: 'b,
    {
        self.module_calls
            .into_iter()
            .flatten()
            .map(move |(name, value)| {
                let mut parent = parent.clone();
                parent.push(value.source);
                let source = parent
                    .canonicalize()
                    .expect("terraform provided incorrect path");
                let _ = source.strip_prefix(base);
                let tree = Tree::new(TreeNode {
                    name,
                    count: value.count_expression.map(|x| x.constant_value),
                    for_each: value.for_each_expression.map(|x| x.constant_value),
                    source,
                })
                .with_leaves(value.module.into_trees(base, parent));
                tree
            })
    }
}

#[derive(Deserialize)]
struct ModuleCall<'a> {
    #[serde(borrow = "'a")]
    module: Module<'a>,
    source: &'a str,
    count_expression: Option<CountExpression>,
    for_each_expression: Option<ForEachExpression<'a>>,
}

#[derive(Deserialize)]
struct CountExpression {
    constant_value: usize,
}

#[derive(Deserialize)]
struct ForEachExpression<'a> {
    #[serde(borrow = "'a")]
    constant_value: HashMap<&'a str, IgnoredAny>,
}

struct TreeNode<'a> {
    name: &'a str,
    count: Option<usize>,
    for_each: Option<HashMap<&'a str, IgnoredAny>>,
    source: PathBuf,
}

impl fmt::Display for TreeNode<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path: PathBuf = self.source.iter().collect();
        let path = path.canonicalize().map_err(|_| fmt::Error)?;
        f.write_str(self.name)?;
        if let Some(index) = self.count {
            write!(f, "[{index}]")?;
        }
        if let Some(for_each) = &self.for_each {
            f.write_char('{')?;
            for (index, each) in for_each.keys().enumerate() {
                write!(f, "{each}")?;
                if index + 1 < for_each.len() {
                    f.write_char(' ')?;
                }
            }
            f.write_char('}')?;
        }
        write!(f, " ({})", path.to_str().ok_or(fmt::Error)?)
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Calculate dirs
    let mut terraform_dir = env::current_dir().context("could not detect current directory")?;
    terraform_dir.push(args.path);
    terraform_dir
        .canonicalize()
        .context("failed to resolve path")?;
    let mut terraform_dir_arg = OsString::from("-chdir=");
    terraform_dir_arg.push(terraform_dir.as_os_str());

    // Create `.plan` path
    let terraform_dir_str = terraform_dir_arg.as_os_str();
    let mut hasher = DefaultHasher::new();
    terraform_dir_str.hash(&mut hasher);
    let plan_name = hasher.finish();
    let mut temp_plan = env::temp_dir();
    temp_plan.push(plan_name.to_string());
    temp_plan.set_extension(".plan");

    // Run `terraform plan` command
    let mut command = process::Command::new("terraform");
    command.arg(&terraform_dir_arg);
    for var_file in args.var_file {
        command.arg("-var-file");
        command.arg(var_file);
    }
    for var in args.var {
        command.arg("-var");
        command.arg(var);
    }
    command
        .args(["plan", "-out"])
        .arg(temp_plan.as_os_str())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Output {
        status,
        stdout,
        stderr,
    } = command
        .output()
        .context("failed to spawn `terraform plan`")?;
    let stdout = String::from_utf8(stdout).context("output not utf-8")?;
    if !status.success() {
        let error = if !stderr.is_empty() {
            String::from_utf8(stderr).context("output not utf-8")?
        } else {
            stdout
        };
        anyhow::bail!(error)
    }

    // Run `terraform show` command
    let mut command = process::Command::new("terraform");
    command.args(["show", "-json"]);
    command.arg(temp_plan);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let Output {
        status,
        stdout,
        stderr,
    } = command
        .output()
        .context("failed to spawn `terraform plan`")?;
    let stdout = String::from_utf8(stdout).context("output not utf-8")?;
    if !status.success() {
        let error = if !stderr.is_empty() {
            String::from_utf8(stderr).context("output not utf-8")?
        } else {
            stdout
        };
        anyhow::bail!(error)
    }

    // Create tree
    let show: Show = serde_json::from_str(&stdout).context("failed to deserialize")?;
    let root_node = TreeNode {
        name: "*",
        count: None,
        for_each: None,
        source: terraform_dir.clone(),
    };
    let tree = Tree::new(root_node).with_leaves(
        show.configuration
            .root_module
            .into_trees(&terraform_dir, terraform_dir.clone())
            .into_iter(),
    );
    print!("{tree}");

    Ok(())
}
