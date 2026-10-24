mod cd;
mod gui;
mod image;
mod iso9660;
mod ops;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use image::Image;
use iso9660::Filesystem;
use ops::{Progress, SaveMode, SaveOptions, TrackFormat};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// comBINe — ferramenta de imagens de CD (BIN/CUE, CCD, ISO) inspirada no CDmage.
/// Sem argumentos abre a interface gráfica.
#[derive(Parser)]
#[command(name = "combine", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Imagem para abrir na interface gráfica
    image: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mostra trilhas e informações do volume
    Info { image: PathBuf },
    /// Lista os arquivos do sistema ISO9660
    Ls { image: PathBuf },
    /// Verifica sync, cabeçalho, EDC e ECC de todos os setores de dados
    Scan { image: PathBuf },
    /// Regenera sync/cabeçalho/EDC/ECC (modifica a imagem!)
    Rebuild {
        image: PathBuf,
        /// Reconstrói todos os setores de dados, não só os que têm erro
        #[arg(long)]
        all: bool,
        /// Confirma a modificação da imagem original
        #[arg(long)]
        yes: bool,
    },
    /// Junta todas as trilhas em um único .bin + .cue
    Merge {
        image: PathBuf,
        /// Pasta de saída
        #[arg(short, long)]
        out: PathBuf,
        /// Nome base dos arquivos (padrão: nome da imagem)
        #[arg(short, long)]
        name: Option<String>,
        /// Regenera EDC/ECC ao copiar (corrige setores na cópia)
        #[arg(long)]
        fix: bool,
    },
    /// Separa cada trilha em um .bin próprio + .cue
    Split {
        image: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        #[arg(short, long)]
        name: Option<String>,
        /// Padrão de nome: ?i nome da imagem, ?n número da trilha, ?e extensão
        #[arg(long, default_value = "?i (Track ?n).?e")]
        pattern: String,
        #[arg(long)]
        fix: bool,
    },
    /// Extrai arquivos (ou tudo) do sistema de arquivos
    Extract {
        image: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        /// Caminhos dentro do disco (padrão: tudo)
        paths: Vec<String>,
        /// Extrai como setores brutos (RIFF CDXA) mesmo arquivos comuns
        #[arg(long)]
        raw: bool,
    },
    /// Extrai uma trilha como BIN, ISO ou WAV
    ExtractTrack {
        image: PathBuf,
        track: u8,
        #[arg(short, long)]
        out: PathBuf,
        /// bin, iso ou wav
        #[arg(short, long, default_value = "bin")]
        format: String,
    },
    /// Substitui o conteúdo de um arquivo dentro da imagem (modifica a imagem!)
    Import {
        image: PathBuf,
        /// Caminho dentro do disco
        path: String,
        /// Arquivo novo
        file: PathBuf,
        #[arg(long)]
        yes: bool,
    },
    /// Salva a área de sistema (16 setores de boot/licença)
    Boot {
        image: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
    },
}

fn run_with_progress<T: Send>(f: impl FnOnce(&Progress) -> Result<T> + Send) -> Result<T> {
    let p = Arc::new(Progress::default());
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    std::thread::scope(|s| {
        let (p2, d2) = (p.clone(), done.clone());
        s.spawn(move || {
            let tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
            while !d2.load(Ordering::Relaxed) {
                if tty {
                    eprint!("\r{}: {:5.1}%", p2.stage.lock().unwrap(), p2.fraction() * 100.0);
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            if tty {
                eprint!("\r\x1b[K");
            }
        });
        let r = f(&p);
        done.store(true, Ordering::Relaxed);
        r
    })
}

fn print_info(img: &Image) {
    println!("Imagem: {}", img.path.display());
    println!("Setores: {} ({})", img.total_sectors(), cd::msf_string(img.total_sectors()));
    println!("{:<4} {:<12} {:>9} {:>9} {:>10}  Arquivo", "Trl", "Modo", "Início", "Setores", "Tamanho");
    for t in &img.tracks {
        println!(
            "{:<4} {:<12} {:>9} {:>9} {:>10}  {}",
            format!("{:02}", t.number),
            t.mode_text,
            cd::msf_string(t.start_lba()),
            t.region_sectors,
            t.region_sectors as u64 * t.sector_size as u64,
            img.files[t.file].path.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    if let Ok(fs) = Filesystem::read(img) {
        let i = &fs.info;
        println!("\nVolume: {}  Sistema: {}  Data: {}", i.volume_id, i.system_id, i.creation_date);
        if let Some(b) = &i.boot {
            println!("Executável de boot (ID do jogo): {b}");
        }
        if let Some(l) = &i.license {
            println!("Licença: {l}");
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let Some(cmd) = cli.cmd else {
        return gui::run(cli.image);
    };
    match cmd {
        Cmd::Info { image } => print_info(&Image::open(&image)?),
        Cmd::Ls { image } => {
            let img = Image::open(&image)?;
            let fs = Filesystem::read(&img)?;
            let mut v = Vec::new();
            fs.root.walk("", &mut v);
            for (path, e) in v {
                println!("{:>10} {:>7} {}  {:<26} {}", if e.is_dir { "<DIR>".into() } else { e.size.to_string() }, e.lba, e.date_string(), e.kind_string(), path);
            }
        }
        Cmd::Scan { image } => {
            let img = Image::open(&image)?;
            let rep = run_with_progress(|p| ops::scan(&img, p))?;
            for t in &rep.skipped_tracks {
                println!("Trilha {t:02}: setores cozidos (2048/2336), sem EDC/ECC para verificar");
            }
            for e in &rep.errors {
                println!("Setor {:>7} ({}) trilha {:02} {}: {}", e.lba, cd::msf_string(e.lba + cd::LBA_OFFSET), e.track, e.form.name(), e.problems.describe());
            }
            println!("{} setores verificados, {} com erro", rep.scanned, rep.errors.len());
            if !rep.errors.is_empty() {
                std::process::exit(2);
            }
        }
        Cmd::Rebuild { image, all, yes } => {
            if !yes {
                bail!("rebuild altera a imagem original; faça um backup e repita com --yes (ou use 'merge --fix' para gerar uma cópia corrigida)");
            }
            let img = Image::open(&image)?;
            let lbas = if all { ops::all_data_lbas(&img) } else { run_with_progress(|p| ops::scan(&img, p))?.errors.iter().map(|e| e.lba).collect() };
            let n = run_with_progress(|p| ops::rebuild_in_place(&img, &lbas, cd::RebuildFields::default(), p))?;
            println!("{n} setor(es) reconstruído(s)");
        }
        Cmd::Merge { image, out, name, fix } => {
            let img = Image::open(&image)?;
            let mut o = SaveOptions::new(&img, out, SaveMode::Merge);
            o.name = name.unwrap_or(o.name);
            o.regenerate = fix;
            let cue = run_with_progress(|p| ops::save(&img, &o, p))?;
            println!("Salvo: {}", cue.display());
        }
        Cmd::Split { image, out, name, pattern, fix } => {
            let img = Image::open(&image)?;
            let mut o = SaveOptions::new(&img, out, SaveMode::Split);
            o.name = name.unwrap_or(o.name);
            o.pattern = pattern;
            o.regenerate = fix;
            let cue = run_with_progress(|p| ops::save(&img, &o, p))?;
            println!("Salvo: {}", cue.display());
        }
        Cmd::Extract { image, out, paths, raw } => {
            let img = Image::open(&image)?;
            let fs = Filesystem::read(&img)?;
            let entries: Vec<&iso9660::Entry> = if paths.is_empty() { vec![&fs.root] } else { paths.iter().map(|p| fs.root.find(p).with_context(|| format!("não encontrado no disco: {p}"))).collect::<Result<_>>()? };
            let warnings = run_with_progress(|p| ops::extract_entries(&img, &fs, &entries, &out, raw, p))?;
            for w in warnings {
                println!("{w}");
            }
            println!("Extraído em {}", out.display());
        }
        Cmd::ExtractTrack { image, track, out, format } => {
            let img = Image::open(&image)?;
            let ti = img.tracks.iter().position(|t| t.number == track).with_context(|| format!("trilha {track} não existe"))?;
            let fmt = match format.to_ascii_lowercase().as_str() {
                "bin" | "raw" => TrackFormat::Raw,
                "iso" => TrackFormat::Iso,
                "wav" => TrackFormat::Wav,
                f => bail!("formato desconhecido: {f}"),
            };
            run_with_progress(|p| ops::extract_track(&img, ti, fmt, &out, p))?;
            println!("Salvo: {}", out.display());
        }
        Cmd::Import { image, path, file, yes } => {
            if !yes {
                bail!("import altera a imagem original; faça um backup e repita com --yes");
            }
            let img = Image::open(&image)?;
            let fs = Filesystem::read(&img)?;
            let e = fs.root.find(&path).with_context(|| format!("não encontrado no disco: {path}"))?;
            fs.import(&img, e, &std::fs::read(&file)?)?;
            println!("{} substituído", e.name);
        }
        Cmd::Boot { image, out } => {
            ops::save_boot(&Image::open(&image)?, &out)?;
            println!("Salvo: {}", out.display());
        }
    }
    Ok(())
}
