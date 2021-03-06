extern crate serde;
extern crate reqwest;
extern crate env_logger;
extern crate serde_json;
extern crate tar;
extern crate flate2;

#[macro_use]
extern crate serde_derive;

#[macro_use]
extern crate log;

mod cache;
mod refresh;

use std::collections::HashMap;
use std::convert::AsRef;
use std::fs;
use std::fs::File;
use std::path::{Path, PathBuf};
use refresh::refresh;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub dependencies: Option<HashMap<String, String>>,

    #[serde(skip_serializing)]
    pub root: Option<PathBuf>,
}

impl Package {
    pub fn load<P: AsRef<Path>>(path: P) -> Package {
        let path = path.as_ref();
        debug!("loading {:?}", path);
        let file = File::open(path.join("package.json")).unwrap();
        let mut package: Package = serde_json::from_reader(file).unwrap();
        package.root = Some(path.to_owned());

        package
    }

    pub fn validate(&self) -> Vec<Issue> {
        fn validate_package(root: &PackageTree, package: &Package, at: &[&String]) -> Vec<Issue> {
            let mut issues = vec![];
            let empty = HashMap::new();
            let deps = package.dependencies.as_ref().unwrap_or(&empty);
            for (name, version) in deps {
                let mut next = at.to_owned();
                next.push(&name);
                let expected_version = version.clone();
                let mut node_issues: Vec<Issue> = match root.get(&name, &next) {
                    Some(dep_node) => {
                        let actual_version = dep_node.package.version.clone();
                        if expected_version != actual_version {
                            return vec![Issue::WrongVersionInstalled {
                                package: name.clone(),
                                expected_version,
                                actual_version,
                            }];
                        }
                        validate_package(root, &dep_node.package, &next)
                    }
                    None => vec![Issue::PackageNotInstalled {
                        package: name.clone(),
                        version: expected_version,
                    }],
                };
                issues.append(&mut node_issues);
            }

            issues
        }

        fn validate_package_lock(
            root: &PackageTree,
            lock: &PackageLock,
            pkg: &Package,
            at: &[&String],
        ) -> Vec<Issue> {
            let mut issues = vec![];
            let empty = HashMap::new();
            let deps = pkg.dependencies.as_ref().unwrap_or(&empty);

            for name in deps.keys() {
                issues.append(&mut match root.get(name, &at) {
                    Some(node) => match lock.get(&name, &at) {
                        Some(_dep) => {
                            let mut next = at.to_vec();
                            next.push(&name);

                            validate_package_lock(root, lock, &node.package, &next)
                        }
                        None => vec![Issue::MissingPackageFromLock {
                            package: name.clone(),
                        }],
                    },
                    None => vec![],
                });
            }

            issues
        }

        let lock = PackageLock::load(self.root.as_ref().unwrap());
        let root = package_file_tree(self.root.as_ref().unwrap());

        let mut issues = validate_package(&root, self, &[]);
        issues.append(&mut validate_package_lock(&root, &lock, &self, &[]));

        issues
    }

    pub fn refresh(&self) {
        refresh(&self);
        // let issues = self.validate();
        // for issue in issues {
        //     match issue {
        //         Issue::PackageNotInstalled{ref package, ref version} => {
        //             cache::extract("tmp/foo", package, version);
        //         },
        //         other => warn!("{:?}", other)
        //     }
        // }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageLock {
    pub name: String,
    pub version: String,
    pub lockfile_version: u8,
    pub description: Option<String>,
    pub dependencies: Option<HashMap<String, PackageLockDependency>>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageLockDependency {
    #[serde(skip_serializing)]
    pub name: Option<String>,

    pub version: String,
    pub resolved: String,
    pub integrity: String,
    pub requires: Option<HashMap<String, String>>,
    pub dependencies: Option<HashMap<String, PackageLockDependency>>,
}

impl PackageLock {
    fn load<P: AsRef<Path>>(path: P) -> PackageLock {
        let path = path.as_ref().join("package-lock.json");
        let file = File::open(path).unwrap();
        let package: PackageLock = serde_json::from_reader(file).unwrap();

        package
    }

    fn get(&self, name: &str, at: &[&String]) -> Option<&PackageLockDependency> {
        match &self.dependencies {
            Some(deps) => find_lock_dependency(&deps, name, &at),
            None => None,
        }
    }
}

fn find_lock_dependency<'a>(
    deps: &'a HashMap<String, PackageLockDependency>,
    name: &str,
    at: &[&String],
) -> Option<&'a PackageLockDependency> {
    if ! at.is_empty() {
        let next = at[0];
        let at = &at[1..].to_vec();
        if let Some(lock) = deps.get(next) {
            if let Some(deps) = &lock.dependencies {
                return find_lock_dependency(deps, name, at)
            }
        }
    }

    deps.get(name)
}

#[derive(Debug)]
pub enum Issue {
    MissingPackageFromLock {
        package: String,
    },
    PackageNotInstalled {
        package: String,
        version: String,
    },
    WrongVersionInstalled {
        package: String,
        expected_version: String,
        actual_version: String,
    },
}

struct PackageTree {
    package: Package,
    children: HashMap<String, PackageTree>,
}

impl PackageTree {
    fn get(&self, name: &str, at: &[&String]) -> Option<&PackageTree> {
        if ! at.is_empty() {
            let next = at[0];
            let at = &at[1..].to_vec();
            if let Some(child) = self.children.get(next) {
                if let Some(node) = child.get(name, at) {
                    return Some(node);
                }
            }
        }

        self.children.get(name)
    }
}

fn package_file_tree<P: AsRef<Path>>(root: P) -> PackageTree {
    let root = root.as_ref();
    let mut node = PackageTree {
        package: Package::load(root),
        children: HashMap::new(),
    };
    let files = match fs::read_dir(root.join("node_modules")) {
        Ok(files) => files.collect(),
        Err(_) => vec![],
    };
    let packages = files
        .into_iter()
        .map(|f| f.unwrap().path())
        .filter(|f| f.is_dir())
        .map(|d| {
            if d.file_name().unwrap().to_str().unwrap().starts_with('@') {
                fs::read_dir(d)
                    .unwrap()
                    .into_iter()
                    .map(|f| f.unwrap().path())
                    .filter(|f| f.is_dir())
                    .collect()
            } else {
                vec![d]
            }
        }).flatten();
    for pkg in packages {
        let child = package_file_tree(pkg);
        node.children.insert(child.package.name.clone(), child);
    }

    node
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_package() {
        let p = Package::load("fixtures/example");
        assert_eq!(p.name, "example");
        assert_eq!(p.version, "0.0.0");
        assert_eq!(p.dependencies.unwrap().get("edon-test-a").unwrap(), "0.0.1");
    }
    #[test]
    fn loads_package_lock() {
        let p = PackageLock::load("fixtures/example");
        assert_eq!(p.name, "example");
        assert_eq!(p.version, "0.0.0");
        assert_eq!(
            p.dependencies.unwrap().get("edon-test-a").unwrap().version,
            "0.0.1"
        );
    }
    #[test]
    fn finds_missing_deps_from_lock() {
        let p = Package::load("fixtures/missing-dep-from-lock");
        let issues = p.validate();
        match &issues[0] {
            Issue::MissingPackageFromLock { ref package } => assert_eq!(package, "edon-test-c"),
            _ => panic!("invalid issue {:?}", &issues[0]),
        }
        assert_eq!(issues.len(), 1);
    }
    #[test]
    fn finds_missing_subdeps_from_lock() {
        let p = Package::load("fixtures/missing-subdep-from-lock");
        let issues = p.validate();
        match &issues[0] {
            Issue::MissingPackageFromLock { ref package } => assert_eq!(package, "edon-test-c"),
            _ => panic!("invalid issue {:?}", &issues[0]),
        }
        assert_eq!(issues.len(), 1);
    }
    #[test]
    fn does_not_error_if_no_deps() {
        let p = Package::load("fixtures/no_deps");
        let issues = p.validate();
        assert_eq!(issues.len(), 0);
    }
    #[test]
    fn test_package_file_tree() {
        let tree = package_file_tree("fixtures/example");
        assert_eq!(tree.package.name, "example");
        assert_eq!(
            tree.children.get("edon-test-a").unwrap().package.name,
            "edon-test-a"
        );
        assert_eq!(
            tree.children.get("edon-test-a").unwrap().package.version,
            "0.0.1"
        );
    }
    #[test]
    fn wrong_package_installed_1() {
        let p = Package::load("fixtures/1-wrong-package-version-installed");
        let issues = p.validate();
        match &issues[0] {
            Issue::WrongVersionInstalled {
                ref package,
                ref expected_version,
                ref actual_version,
            } => {
                assert_eq!(package, "edon-test-c");
                assert_eq!(expected_version, "0.0.0");
                assert_eq!(actual_version, "0.0.1");
            }
            _ => panic!("invalid issue"),
        }
        assert_eq!(issues.len(), 1);
    }
    #[test]
    fn valid_multiple_versions() {
        let p = Package::load("fixtures/2-valid-multiple-versions");
        let issues = p.validate();
        assert_eq!(issues.len(), 0);
    }
    #[test]
    fn dep_not_installed_3() {
        let p = Package::load("fixtures/3-dep-not-installed");
        let issues = p.validate();
        match &issues[0] {
            Issue::PackageNotInstalled { ref package, ref version } => {
                assert_eq!(package, "edon-test-c");
                assert_eq!(version, "^1.0.0");
            }
            _ => panic!("invalid issue"),
        }
        assert_eq!(issues.len(), 1);
    }
    #[test]
    fn test_refresh() {
        fs::remove_dir_all("fixtures/3-dep-not-installed/node_modules").unwrap_or(());
        let p = Package::load("fixtures/3-dep-not-installed");
        p.refresh();
        let pjson = Package::load("fixtures/3-dep-not-installed/node_modules/edon-test-c");
        assert_eq!(pjson.version, "1.0.4");
        fs::remove_dir_all("fixtures/3-dep-not-installed/node_modules").unwrap_or(());
    }
}
