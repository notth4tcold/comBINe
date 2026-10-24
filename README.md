# comBINe

Ferramenta de imagens de CD para Linux (Arch e Ubuntu), escrita em Rust, que
recria as funções do **CDmage 1.02.1** (Windows): abrir imagens BIN/CUE, juntar
tudo em um único `.bin`, separar trilhas, verificar e reconstruir EDC/ECC,
navegar e extrair arquivos do disco (ISO9660 + CD-ROM XA do PlayStation) e
substituir arquivos dentro da imagem.

Tem interface gráfica (abre com dois cliques, arrastar e soltar funciona) e
também linha de comando para scripts.

## Instalação

Baixe o pacote da sua distro na página de
[Releases](https://github.com/notth4tcold/comBINe/releases/latest):

- **Arch Linux**: `sudo pacman -U combine-*-x86_64.pkg.tar.zst`
- **Ubuntu 22.04 ou mais novo**: `sudo apt install ./combine_*_amd64.deb`

Ou compile a partir do código:

### Arch Linux
```bash
git clone https://github.com/notth4tcold/comBINe.git
cd comBINe/packaging
makepkg -si          # gera e instala o pacote "combine"
```
Se o seu Rust veio do rustup/asdf (e não do pacman), use `makepkg -sid` para o
pacman não tentar instalar o pacote `rust` de novo.

### Ubuntu
```bash
sudo apt install build-essential curl
curl https://sh.rustup.rs -sSf | sh          # se ainda não tiver Rust
cd comBINe && ./install.sh                   # instala em ~/.local
# ou, para gerar um .deb:
cargo install cargo-deb && cargo deb && sudo apt install ./target/debian/combine_*.deb
```

### Qualquer distro (sem pacote)
```bash
./install.sh             # compila e instala em ~/.local (atalho no menu incluso)
./install.sh --uninstall
```

As janelas de "Abrir/Salvar" usam o portal do desktop
(`xdg-desktop-portal-gtk` ou `-kde`), já presente no Ubuntu e no Hyprland/KDE.

## Uso na interface

- **Abrir**: `.cue`, `.ccd` (CloneCD), `.bin`, `.img`, `.iso`, `.mm2`. Também dá
  para arrastar o arquivo para a janela.
- **Juntar em um BIN**: salva todas as trilhas em um único `.bin` + `.cue`
  (formato que o PSIO, a maioria dos emuladores e o "Save As" do CDmage geram).
  Os índices (INDEX 00/01, PREGAP) são recalculados.
- **Separar trilhas**: um `.bin` por trilha (padrão Redump `Nome (Track 01).bin`;
  o padrão é configurável com `?i`, `?n`, `?e` como no CDmage).
- Trilhas "cozidas" (ISO 2048, MODE2/2336, 2448 com subcanal) são convertidas para
  2352 bytes com sync/cabeçalho/EDC/ECC gerados.
- **Verificar**: checa sync, endereço MSF, modo, subheader, EDC e ECC P/Q de
  todos os setores de dados; os erros aparecem no log (clique para ver o setor).
- **Reconstruir**: regenera os campos escolhidos (só nos setores com erro ou em
  todos). Modifica a imagem — para uma cópia corrigida use "Juntar em um BIN"
  com "Regenerar EDC/ECC".
- **Arquivos**: navega pelas pastas, extrai arquivos/pastas (com datas
  originais), e "Importar sobre..." substitui um arquivo (tem que caber no espaço
  original). Arquivos de vídeo/áudio XA (STR/XA, Form 2) são extraídos como RIFF
  CDXA com setores de 2352 bytes, como o CDmage faz.
- **Setores**: visualizador hexadecimal com os campos coloridos nas cores padrão
  do CDmage (sync, cabeçalho, subheader, dados, EDC, ECC P, ECC Q).
- **Informações**: ID do jogo (via `SYSTEM.CNF`), licença/região, dados do volume
  e trilhas, com botões para extrair cada trilha como BIN, ISO ou WAV.

## Linha de comando

```bash
combine                                  # abre a interface
combine jogo.cue                         # abre a interface com a imagem
combine info  jogo.cue
combine ls    jogo.cue
combine scan  jogo.cue                   # código de saída 2 se houver erros
combine merge jogo.cue -o saida/         # um único .bin + .cue
combine merge jogo.cue -o saida/ --fix   # cópia com EDC/ECC regenerados
combine split jogo.cue -o saida/ --pattern "?i (Track ?n).?e"
combine extract jogo.cue -o pasta/ [CAMINHO...]
combine extract-track jogo.cue 2 -o faixa2.wav -f wav
combine rebuild jogo.cue --yes [--all]   # modifica a imagem!
combine import jogo.cue SYSTEM.CNF novo.cnf --yes
combine boot jogo.cue -o boot.img
```

## Como o CDmage foi analisado

O executável `CDmage.exe` (SHA-256 `8fc5f404…f9f0`) é um programa Delphi (VCL)
Win32; o `CDmage1-02-1B5.exe` é um pacote autoextraível (UPX + ZIP) com um
`CDmage.exe` dentro (não verifiquei se é byte a byte o mesmo). O que foi observado no binário:

- **Formatos** (filtros de diálogo e tabelas de strings): CUE (CDRWin/Fireburner),
  CloneCD `.ccd`, Nero `.nrg`, DiscJuggler `.cdi`, BlindWrite, NTI, Easy CD
  Creator, Prassi, imagens brutas `.iso/.bin/.img/.mm2`, e os modos de trilha
  `MODE1/2048…2448`, `MODE2/2336…2448`, `CDI/2336|2352`, `AUDIO`, `CDG`.
- **Funções** (nomes de formulários/menus): `CDSaveAs` ("Save tracks to cue
  file", "Save first session only", "Convert tracks"), `ScanDlg` ("Scan for
  corruption"), `miRebuild` ("Rebuild Sector Fields", "Select fields to rebuild"),
  `CDExtract`/`ExtractBootDlg`, `CDImport` ("Import to the sectors in the Error
  Log"), comparação de pastas, log de erros e de arquivos encontrados.
- **EDC**: a rotina em `0x493216` gera a tabela CRC com o polinômio `0x8001801B`
  (EDC do ECMA-130) e as tabelas de log/antilog de GF(2⁸) usadas pelo ECC
  Reed-Solomon P/Q. O comBINe usa a forma refletida equivalente (`0xD8018001`);
  um teste unitário confirma que as tabelas são idênticas.
- **Cores e opções** vêm do `CDmage.ini` (`[Colors]`, `NamingScheme=?i track ?n
  ?m-?s.?e`, extensões padrão por modo de trilha).

Os setores gerados pelo comBINe foram validados com o ECM de Neill Corlett
(implementação de referência): todos os setores Mode 1, Mode 2 Form 1 e Form 2
foram reconhecidos como válidos.

### O que ficou de fora (por enquanto)

- Leitura de `.nrg`, `.cdi`, `.mds` e outros formatos proprietários (use CUE,
  CCD ou ISO; o `chdman`/`iat` convertem os demais).
- Discos multissessão, Joliet, comparação de pastas, reprodução de áudio e leitura
  direta de drive físico.
- Importação de arquivos maiores que o espaço original (exigiria reconstruir o
  sistema de arquivos).

## Desenvolvimento

```bash
cargo test --release     # testes de ponta a ponta com um disco PS1 sintético
cargo build --release
```
