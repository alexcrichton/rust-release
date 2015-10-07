extern crate curl;
extern crate getopts;
extern crate rustc_serialize;

use std::str;
use std::env;
use std::fs::{self, File};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use curl::http::{Handle, Request, Response};
use rustc_serialize::{json, Decodable, Encodable};

macro_rules! t {
    ($e:expr) => (match $e {
        Ok(e) => e,
        Err(e) => panic!("{} failed with {}", stringify!($e), e),
    })
}

#[derive(RustcDecodable)]
struct Release {
    id: u64,
    name: String,
    upload_url: String,
    assets_url: String,
}

fn main() {
    let mut opts = getopts::Options::new();
    opts.optflag("h", "help", "Show this help message");
    opts.optopt("p", "project", "Build specified project", "DIR");
    opts.optopt("d", "docker", "Docker container for linux", "TAG");
    opts.optopt("t", "token", "GitHub auth token", "TOKEN");
    opts.optopt("r", "repo", "GitHub repository to publish to", "REPO");

    let matches = match opts.parse(env::args().skip(1)) {
        Ok(m) => m,
        Err(e) => {
            println!("failed to parse arguments: {}", e);
            return usage(&opts);
        }
    };
    if matches.opt_present("h") {
        return usage(&opts);
    }

    let token = flagorenv(&matches, "t", &["GH_TOKEN", "TOKEN"]);
    let repo = flagorenv(&matches, "r", &["TRAVIS_REPO_SLUG"]);

    let rustc = t!(Command::new("rustc").arg("-vV").output());
    assert!(rustc.status.success());
    let info = t!(String::from_utf8(rustc.stdout));

    let host = info.lines().find(|l| l.starts_with("host: ")).unwrap();
    let host = &host.trim()[6..];

    let project = matches.opt_str("p").map(PathBuf::from)
                         .unwrap_or_else(|| t!(env::current_dir()));
    // if host.contains("unknown-linux-gnu") {
    //     let default = "alexcrichton/rust-centos-dist".to_string();
    //     let docker = matches.opt_str("d").unwrap_or(default);
    //     build_linux(&project, &docker);
    // } else if host.contains("apple-darwin") {
    //     build_macos(&project);
    // } else {
    //     panic!("unknown host: {}", host);
    // }

    publish(&project, &repo, &token, host);
}

fn usage(opts: &getopts::Options) {
    let prog = env::args().next().unwrap();
    println!("{}", opts.usage(&format!("Usage: {} [options]", prog)));
}

fn build_linux(project: &Path, container: &str) {
    let root = t!(Command::new("rustc").arg("--print").arg("sysroot").output());
    let root = t!(String::from_utf8(root.stdout));
    run(Command::new("docker").arg("pull").arg(container));

    let mut mount1 = OsString::from(root.trim());
    mount1.push(":/rust:ro");
    let mut mount2 = OsString::from(project);
    mount2.push(":/home/rustbuild");
    run(Command::new("docker").arg("run")
                .arg("-v").arg(mount1)
                .arg("-v").arg(mount2)
                .arg("-it").arg(container)
                .arg("cargo").arg("build").arg("--release"));
}

fn build_macos(project: &Path) {
    let mut cmd = Command::new("cargo");
    cmd.arg("build").arg("--release").current_dir(project);
    if env::var("MACOSX_DEPLOYMENT_TARGET").is_err() {
        cmd.env("MACOSX_DEPLOYMENT_TARGET", "10.7");
    }
    run(&mut cmd);
}

fn publish(project: &Path, repo: &str, token: &str, host: &str) {
    let mut handle = Handle::new();
    let release = get_release(&mut handle, repo, token);

    let sha = t!(Command::new("git").arg("rev-parse").arg("HEAD").output());
    let sha = t!(String::from_utf8(sha.stdout));

    update_release(&mut handle, &release, repo, token, sha.trim());
    println!("release: {}", release.id);
    handle = Handle::new(); // ... why? otherwise listing assets fails...

    for file in t!(fs::read_dir(project.join("target/release"))) {
        let file = t!(file);
        if !t!(file.file_type()).is_file() {
            continue
        }
        upload(&mut handle, &release, repo, token, host, &file.path());
    }
}

fn get_release(handle: &mut Handle, repo: &str, token: &str) -> Release {
    let url = format!("https://api.github.com/repos/{}/releases", repo);
    let releases: Vec<Release> = json(handle.get(&url[..]), token);
    for release in releases {
        if release.name == "master" {
            return release
        }
    }

    #[derive(RustcEncodable)]
    struct Create {
        tag_name: String,
        name: String,
        draft: bool,
    }
    let body = t!(json::encode(&Create {
        tag_name: "master".to_string(),
        name: "master".to_string(),
        draft: true,
    }));
    let r: Release = json(handle.post(&url[..], &body), token);
    return r
}

fn update_release(handle: &mut Handle, release: &Release, repo: &str,
                  token: &str, sha: &str) {
    #[derive(RustcEncodable)]
    struct Update {
        target_commitish: String,
        draft: bool,
    }
    let url = format!("https://api.github.com/repos/{}/releases/{}", repo,
                      release.id);
    let body = t!(json::encode(&Update {
        target_commitish: sha.to_string(),
        draft: false,
    }));
    json::<Release>(handle.patch(&url[..], &body), token);
}

fn upload(handle: &mut Handle, release: &Release, repo: &str, token: &str,
          host: &str, path: &Path) {
    #[derive(RustcDecodable)]
    struct Asset {
        id: u64,
        name: String,
        label: String,
    }
    println!("fetching assets: {:?}", release.assets_url);
    let v: Vec<Asset> = json(handle.get(&release.assets_url[..]), token);
    let stem = path.file_stem().unwrap().to_str().unwrap();
    let filename = format!("{}-{}{}", stem, host, env::consts::EXE_SUFFIX);
    for asset in v {
        if asset.name == filename {
            let url = format!("https://api.github.com/repos/{}/releases/assets/{}",
                              repo, asset.id);
            println!("deleting previous asset: {}", url);
            exec(handle.delete(&url[..]), token);
            break
        }
    }

    let mut file = File::open(path).unwrap();
    let meta = fs::metadata(path).unwrap();
    let upload_url = &release.upload_url[..release.upload_url.find("{").unwrap()];
    let url = format!("{}?name={}", upload_url, filename);
    println!("upload to: {}", url);
    let req = handle.post(&url[..], &mut file)
                    .content_length(meta.len() as usize);
    json::<Asset>(req.header("Content-Type", "application/octet-stream"), token);

}

fn flagorenv(matches: &getopts::Matches, flag: &str, env: &[&str]) -> String {
    if let Some(s) = matches.opt_str(flag) {
        return s
    }
    for var in env {
        if let Ok(s) = env::var(var) {
            return s
        }
    }
    panic!("requires either -{} or one of {}", flag, env.join(", "));
}

fn run(cmd: &mut Command) {
    println!("running {:?}", cmd);
    let status = t!(cmd.status());
    assert!(status.success());
}

fn json<T: Decodable>(req: Request, token: &str) -> T {
    let body = exec(req, token);
    let json = t!(str::from_utf8(body.get_body()));
    t!(json::decode(json))
}

fn exec(req: Request, token: &str) -> Response {
    let body = t!(req.header("Authorization", &format!("token {}", token))
                     .header("User-Agent", "rust-release")
                     .header("Accept", "application/vnd.github+json")
                     .exec());
    if body.get_code() < 200 || body.get_code() >= 300 {
        panic!("failed to get 200: {}", body);
    }
    body
}
