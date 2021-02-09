use anyhow::Context;
use pubgrub::error::PubGrubError;
use pubgrub::range::Range;
use pubgrub::report::{DefaultStringReporter, Reporter};
use pubgrub::solver::resolve;
use pubgrub::type_aliases::Map;
use pubgrub::version::SemanticVersion as SemVer;
use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

use pubgrub_dependency_provider_elm::constraint::Constraint;
use pubgrub_dependency_provider_elm::dependency_provider::{
    ElmPackageProviderOffline, ElmPackageProviderOnline, ProjectAdapter, VersionStrategy,
};
use pubgrub_dependency_provider_elm::project_config::{
    AppDependencies, ApplicationConfig, PackageConfig, ProjectConfig,
};

#[derive(Debug)]
pub enum ConnectivityStrategy {
    Progressive,
    Offline,
    Online(VersionStrategy),
}

impl FromStr for ConnectivityStrategy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "progressive" => Ok(Self::Progressive),
            "offline" => Ok(Self::Offline),
            "online-newest" => Ok(Self::Online(VersionStrategy::Newest)),
            "online-oldest" => Ok(Self::Online(VersionStrategy::Oldest)),
            _ => Err(format!("Invalid connectivity option: {}", s)),
        }
    }
}

/// Install elm-explorations/test to the tests dependencies.
pub fn init(config: ProjectConfig) -> anyhow::Result<ProjectConfig> {
    match config {
        ProjectConfig::Application(app_config) => Ok(ProjectConfig::Application(
            init_app(app_config).context("Error while setting up the app test dependencies")?,
        )),
        ProjectConfig::Package(pkg_config) => Ok(ProjectConfig::Package(
            init_pkg(pkg_config).context("Error while setting up the package test dependencies")?,
        )),
    }
}

fn init_app(mut app_config: ApplicationConfig) -> anyhow::Result<ApplicationConfig> {
    // Retrieve all direct and indirect dependencies
    let indirect_test_deps = app_config.test_dependencies.indirect.iter();
    let mut all_deps: Map<String, Range<SemVer>> = indirect_test_deps
        .chain(app_config.dependencies.indirect.iter())
        .chain(app_config.test_dependencies.direct.iter())
        .chain(app_config.dependencies.direct.iter())
        .map(|(p, v)| (p.clone(), Range::exact(*v)))
        .collect();

    // Check that those dependencies are correct
    solve_check(&all_deps, true).context("The app dependencies are incorrect")?;

    // Check if elm-explorations/test is already in the dependencies.
    let test_pkg = "elm-explorations/test".to_string();
    if all_deps.contains_key(&test_pkg) {
        if app_config
            .test_dependencies
            .indirect
            .contains_key(&test_pkg)
        {
            eprintln!("elm-explorations/test is already in your indirect test dependencies,");
            eprintln!("so we just upgrade it to a direct test dependency.");
            let v = app_config
                .test_dependencies
                .indirect
                .remove(&test_pkg)
                .unwrap(); // this unwrap is fine since we check existence just before.
            app_config.test_dependencies.direct.insert(test_pkg, v);
        } else if app_config.dependencies.indirect.contains_key(&test_pkg) {
            eprintln!("elm-explorations/test is already in your indirect dependencies,");
            eprintln!("so we copied the same version in your direct test dependencies.");
            let v = app_config.dependencies.indirect.get(&test_pkg).unwrap(); // this unwrap is fine since we check existence just before.
            app_config.test_dependencies.direct.insert(test_pkg, *v);
        } else {
            eprintln!("elm-explorations/test is already in your dependencies.");
        }
        return Ok(app_config);
    }

    // Add elm-explorations/test to the dependencies
    all_deps.insert(test_pkg.clone(), Range::between((1, 0, 0), (2, 0, 0)));

    // Solve dependencies
    let solution = solve_deps(
        &ConnectivityStrategy::Progressive,
        &all_deps,
        "root".to_string(),
        SemVer::zero(),
    )
    .context("Adding elm-explorations/test to the dependencies failed")?;

    // Add the selected elm-explorations/test version to direct tests deps
    let test_version = solution.get(&test_pkg).unwrap(); // this unwrap is fine since test_pkg was inserted in all_deps just before.
    app_config
        .test_dependencies
        .direct
        .insert(test_pkg, *test_version);

    // Add all other new deps to indirect tests deps
    for (p, v) in solution.into_iter() {
        if !all_deps.contains_key(&p) && &p != "root" {
            app_config.test_dependencies.indirect.insert(p, v);
        }
    }
    Ok(app_config)
}

fn init_pkg(mut pkg_config: PackageConfig) -> anyhow::Result<PackageConfig> {
    // Retrieve all dependencies
    let test_deps = pkg_config.test_dependencies.iter();
    let mut all_deps: Map<String, Range<SemVer>> = test_deps
        .chain(pkg_config.dependencies.iter())
        .map(|(p, c)| (p.clone(), c.0.clone()))
        .collect();

    // Check that those dependencies are correct
    solve_check(&all_deps, false).context("The package dependencies are incorrect")?;

    // Check if elm-explorations/test is already in the dependencies.
    let test_pkg = "elm-explorations/test".to_string();
    if all_deps.contains_key(&test_pkg) {
        eprintln!("elm-explorations/test is already in your dependencies.");
        return Ok(pkg_config);
    }

    // Add elm-explorations/test to the dependencies
    let test_range = Range::between((1, 0, 0), (2, 0, 0));
    all_deps.insert(test_pkg.clone(), test_range.clone());

    // Solve dependencies to check that elm-explorations/test is compatible
    solve_deps(
        &ConnectivityStrategy::Progressive,
        &all_deps,
        pkg_config.name.clone(),
        SemVer::zero(),
    )
    .context("Adding elm-explorations/test to the dependencies failed")?;

    // Add elm-explorations/test to tests deps
    pkg_config
        .test_dependencies
        .insert(test_pkg, Constraint(test_range));
    Ok(pkg_config)
}

/// Solve dependencies needed to run the tests.
pub fn solve<P: AsRef<Path>>(
    connectivity: &ConnectivityStrategy,
    config: &ProjectConfig,
    src_dirs: &[P],
) -> anyhow::Result<ApplicationConfig> {
    match config {
        ProjectConfig::Application(app_config) => {
            let normal_deps = app_config.dependencies.direct.iter();
            let direct_deps: Map<String, Range<SemVer>> = normal_deps
                .chain(app_config.test_dependencies.direct.iter())
                .map(|(p, v)| (p.clone(), Range::exact(*v)))
                .collect();
            // TODO: take somehow into account already picked versions for indirect deps.
            solve_helper(
                connectivity,
                src_dirs,
                &"root".to_string(),
                SemVer::zero(),
                direct_deps,
            )
        }
        ProjectConfig::Package(pkg_config) => {
            let normal_deps = pkg_config.dependencies.iter();
            let deps: Map<String, Range<SemVer>> = normal_deps
                .chain(pkg_config.test_dependencies.iter())
                .map(|(p, c)| (p.clone(), c.0.clone()))
                .collect();
            solve_helper(
                connectivity,
                src_dirs,
                &pkg_config.name,
                pkg_config.version,
                deps,
            )
        }
    }
}

#[allow(clippy::ptr_arg)]
fn solve_helper<P: AsRef<Path>>(
    connectivity: &ConnectivityStrategy,
    src_dirs: &[P],
    pkg_id: &String,
    version: SemVer,
    direct_deps: Map<String, Range<SemVer>>,
) -> anyhow::Result<ApplicationConfig> {
    // TODO: there might be an issue if that was already in the dependencies.
    let mut deps = direct_deps;
    deps.insert(
        "mpizenberg/elm-test-runner".to_string(),
        Range::exact((4, 0, 1)),
    );
    // Add elm/json to the deps since it's used in Runner.elm and Reporter.elm.
    if !deps.contains_key("elm/json") {
        // TODO: maybe not the best way to handle but should work most of the time.
        deps.insert("elm/json".to_string(), Range::between((1, 0, 0), (2, 0, 0)));
    }
    let mut solution = solve_deps(connectivity, &deps, pkg_id.clone(), version)
        .context("Combining the project dependencies with the ones of the test runner failed")?;
    solution.remove(pkg_id);

    // Split solution into direct and indirect deps.
    let dependencies = AppDependencies {
        direct: solution
            .clone()
            .into_iter()
            .filter(|(d, _)| deps.contains_key(d))
            .collect(),
        indirect: solution
            .into_iter()
            .filter(|(d, _)| !deps.contains_key(d))
            .collect(),
    };
    let test_dependencies = AppDependencies {
        direct: BTreeMap::new(),
        indirect: BTreeMap::new(),
    };
    let source_directories: Vec<String> = src_dirs
        .iter()
        .filter_map(|p| p.as_ref().to_str())
        .map(|s| s.to_string())
        .collect();
    Ok(ApplicationConfig {
        source_directories,
        // TODO: might have to change that
        elm_version: SemVer::new(0, 19, 1),
        dependencies,
        test_dependencies,
    })
}

/// Check that those dependencies are correct.
/// Use progressive connectivity mode.
fn solve_check(deps: &Map<String, Range<SemVer>>, is_app: bool) -> anyhow::Result<()> {
    let pkg_id = "root".to_string();
    let version = SemVer::zero();
    let mut solution = solve_deps(
        &ConnectivityStrategy::Progressive,
        deps,
        pkg_id.clone(),
        version,
    )?;
    // Check that indirect deps are correct if this is for an application.
    // All packages in the solution must exist in the original dependencies.
    if is_app {
        solution.remove(&pkg_id);
        for p in solution.keys() {
            if !deps.contains_key(p) {
                anyhow::bail!("{} is missing in the indirect dependencies", p);
            }
        }
    }
    Ok(())
}

/// Solve project dependencies.
fn solve_deps(
    connectivity: &ConnectivityStrategy,
    deps: &Map<String, Range<SemVer>>,
    pkg_id: String,
    version: SemVer,
) -> anyhow::Result<Map<String, SemVer>> {
    let solution = |resolution| match resolution {
        Ok(sol) => Ok(sol),
        Err(PubGrubError::NoSolution(tree)) => {
            Err(anyhow::anyhow!(DefaultStringReporter::report(&tree)))
        }
        Err(PubGrubError::ErrorRetrievingDependencies {
            package,
            version,
            source,
        }) => Err(anyhow::anyhow!(
            "An error occured while trying to retrieve dependencies of {}@{}:\n\n{}",
            package,
            version,
            source
        )),
        Err(PubGrubError::DependencyOnTheEmptySet {
            package,
            version,
            dependent,
        }) => Err(anyhow::anyhow!(
            "{}@{} has an imposible dependency on {}",
            package,
            version,
            dependent
        )),
        Err(PubGrubError::SelfDependency { package, version }) => Err(anyhow::anyhow!(
            "{}@{} somehow depends on itself",
            package,
            version
        )),
        Err(PubGrubError::ErrorChoosingPackageVersion(err)) => Err(anyhow::anyhow!(
            "There was an error while picking packages for dependency resolution:\n\n{}",
            err
        )),
        Err(PubGrubError::ErrorInShouldCancel(err)) => Err(anyhow::anyhow!(
            "Dependency resolution was cancelled.\n\n{}",
            err
        )),
        Err(PubGrubError::Failure(err)) => Err(anyhow::anyhow!(
            "An unrecoverable error happened while solving dependencies:\n\n{}",
            err
        )),
    };
    match connectivity {
        ConnectivityStrategy::Offline => {
            let offline_provider = ElmPackageProviderOffline::new(
                crate::utils::elm_home().context("Elm home not found")?,
                "0.19.1",
            );
            let deps_provider =
                ProjectAdapter::new(pkg_id.clone(), version, deps, &offline_provider);
            solution(resolve(&deps_provider, pkg_id, version))
        }
        ConnectivityStrategy::Online(strategy) => {
            let online_provider = match ElmPackageProviderOnline::new(
                crate::utils::elm_home().context("Elm home not found")?,
                "0.19.1",
                "https://package.elm-lang.org",
                crate::utils::http_fetch,
                strategy.clone(),
            ) {
                Ok(provider) => provider,
                Err(e) => anyhow::bail!("Failed to initialize the online provider.\n{}", e,),
            };
            // TODO: Improve the pubgrub_dependency_provider_elm package to have
            // correctly implemented errors with thiserror.
            let deps_provider =
                ProjectAdapter::new(pkg_id.clone(), version, deps, &online_provider);
            solution(resolve(&deps_provider, pkg_id, version))
        }
        ConnectivityStrategy::Progressive => solve_deps(
            &ConnectivityStrategy::Offline,
            deps,
            pkg_id.clone(),
            version,
        )
        .or_else(|_| {
            solve_deps(
                &ConnectivityStrategy::Online(VersionStrategy::Newest),
                deps,
                pkg_id,
                version,
            )
        }),
    }
}
