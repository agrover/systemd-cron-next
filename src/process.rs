use std::convert::AsRef;
use std::fs::{walk_dir, PathExt};
use std::path::{Path, PathBuf};
use std::collections::{BTreeMap, BTreeSet};
use std::slice::SliceConcatExt;
use std::fmt::Display;

use cronparse::{CrontabFile, CrontabFileError, CrontabFileErrorKind, Limited};
use cronparse::crontab::{EnvVarEntry, CrontabEntry, ToCrontabEntry};
use cronparse::crontab::{SystemCrontabEntry, UserCrontabEntry};
use cronparse::schedule::{Schedule, Period, Calendar, DayOfWeek, Month, Day, Hour, Minute};
use cronparse::interval::Interval;

pub fn process_crontab_dir<T: ToCrontabEntry, D: AsRef<Path>>(srcdir: &str, dstdir: D) {
    let files = walk_dir(srcdir).and_then(|fs| fs.map(|r| r.map(|p| p.path()))
                                       .filter(|r| r.as_ref().map(|p| p.is_file()).unwrap_or(true))
                                       .collect::<Result<Vec<PathBuf>, _>>());
    match files {
        Err(err) => error!("error processing directory {}: {}", srcdir, err),
        Ok(files) => for file in files {
            process_crontab_file::<T, _, _>(file, dstdir.as_ref());
        }
    }
}


pub fn process_crontab_file<T: ToCrontabEntry, P: AsRef<Path>, D: AsRef<Path>>(path: P, dstdir: D) {
    CrontabFile::<T>::new(path.as_ref()).map(|crontab| {
        let mut env = BTreeMap::new();
        for entry in crontab {
            match entry {
                Ok(CrontabEntry::EnvVar(EnvVarEntry(name, value))) => { env.insert(name, value); },
                Ok(data) => generate_systemd_units(data, &env, path.as_ref(), dstdir.as_ref()),
                Err(err @ CrontabFileError { kind: CrontabFileErrorKind::Io(_), .. }) => warn!("error accessing file {}: {}", path.as_ref().display(), err),
                Err(err @ CrontabFileError { kind: CrontabFileErrorKind::Parse(_), .. }) => warn!("skipping file {} due to parsing error: {}", path.as_ref().display(), err),
            }
        }
    }).unwrap_or_else(|err| {
        error!("error parsing file {}: {}", path.as_ref().display(), err);
    });
}

#[allow(non_snake_case)]
fn generate_systemd_units(entry: CrontabEntry, env: &BTreeMap<String, String>, path: &Path, dstdir: &Path) {
    use cronparse::crontab::CrontabEntry::*;

    info!("{} => {:?}, {:?}", path.display(), entry, env);

    let mut persistent = env.get("PERSISTENT").and_then(|v| match &**v {
        "yes" | "true" | "1" => Some(true),
        "auto" | "" => None,
        _ => Some(false)
    }).unwrap_or_else(|| match entry {
        Anacron(_) | User(UserCrontabEntry { sched: Schedule::Period(_), .. }) | System(SystemCrontabEntry { sched: Schedule::Period(_), .. }) => true,
        _ => false
    });

    let batch = env.get("BATCH").map(|v| match &**v {
        "yes" | "true" | "1" => true,
        _ => false
    }).unwrap_or(false);

    let random_delay = env.get("RANDOM_DELAY").and_then(|v| v.parse::<u64>().ok()).unwrap_or(1);
    let mut delay = env.get("DELAY").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let hour = env.get("START_HOURS_RANGE").and_then(|v| v.splitn(1, '-').next().and_then(|v| v.parse::<u64>().ok())).unwrap_or(0);

    let schedule = entry.period().and_then(|period| match *period {
        Period::Reboot => {
            persistent = false;
            if delay == 0 {
                delay = 1;
            }
            None
        },
        Period::Minutely => {
            persistent = false;
            Some("@minutely".to_string())
        },
        Period::Hourly => {
            if delay == 0 {
                Some("@hourly".to_string())
            } else {
                Some(format!("*-*-* *:{}:0", delay))
            }
        },
        Period::Midnight => {
            if delay == 0 {
                Some("@daily".to_string())
            } else {
                Some(format!("*-*-* 0:{}:0", delay))
            }
        },
        Period::Daily => {
            if delay == 0 && hour == 0 {
                Some("@daily".to_string())
            } else {
                Some(format!("*-*-* {}:{}:0", hour, delay))
            }
        },
        Period::Weekly => {
            if delay == 0 && hour == 0 {
                Some("@weekly".to_string())
            } else {
                Some(format!("Mon *-*-* {}:{}:0", hour, delay))
            }
        },
        Period::Monthly => {
            if delay == 0 && hour == 0 {
                Some("@monthly".to_string())
            } else {
                Some(format!("*-*-1 {}:{}:0", hour, delay))
            }
        },
        Period::Quaterly => {
            if delay == 0 && hour == 0 {
                Some("@quaterly".to_string())
            } else {
                Some(format!("*-1,4,7,10-1 {}:{}:0", hour, delay))
            }
        },
        Period::Biannually => {
            if delay == 0 && hour == 0 {
                Some("@semi-annually".to_string())
            } else {
                Some(format!("*-1,7-1 {}:{}:0", hour, delay))
            }
        },
        Period::Yearly => {
            if delay == 0 && hour == 0 {
                Some("@yearly".to_string())
            } else {
                Some(format!("*-1-1 {}:{}:0", hour, delay))
            }
        },
        Period::Days(days) => {
            // workaround for anacrontab
            if days > 31 {
                Some(format!("*-1/{}-1 {}:{}:0", days / 30, hour, delay))
            } else {
                Some(format!("*-*-1/{} {}:{}:0", days, hour, delay))
            }
        },
    }).or_else(|| entry.calendar().and_then(|cal| {
        let Calendar {
            ref dows,
            ref days,
            ref mons,
            ref hrs,
            ref mins
        } = *cal;

        Some(format!("{} *-{}-{} {}:{}:00",
                     linearize(&**dows),
                     linearize(&**mons),
                     linearize(&**days),
                     linearize(&**hrs),
                     linearize(&**mins)))
    }));

    println!("schedule: {:?}", schedule);
}

fn linearize<T: Limited + Display>(input: &[Interval<T>]) -> String {
    if input.len() == 1 && input[0] == Interval::Full(1) {
        "*".to_string()
    } else {
        let mut output = String::new();
        for part in input.iter().flat_map(|v| v.iter()).collect::<BTreeSet<_>>().iter() {
            output.push_str(&*part.to_string());
            output.push(',');
        }
        output.pop();
        output
    }
}
