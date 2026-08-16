<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

**Una sola cabina de mando para Claude Code, Codex, Gemini CLI y los demás CLIs de programación con IA que ya usas.**<br>
Lánzalos, obsérvalos y dirígelos desde una única aplicación nativa en macOS, Windows y Linux.

[English](README.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md) | [한국어](README.ko.md) | [Português (Brasil)](README.pt-BR.md) | **Español**

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

[**Descargar**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**Inicio rápido**](#inicio-rápido) ·
[**Documentación**](#documentación) ·
[**Sitio web**](https://zaivern.com/)

<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Zaivern Code ejecutando Claude Code, Codex, Gemini CLI y otros agentes de programación lado a lado" />
</a>

<!-- 出典: docs/conflict-zero.md §3.12 — zaivern-code / 書き手 16 / zai 0.14.0:
     素の git 26 ファイル・28 ハンク、zaivern あり 0/0・96/96 成立・拒否 0・30 件ずらし -->
**16 agentes escribiendo en este repositorio en paralelo** — git a secas: **26 archivos en conflicto / 28 hunks**.<br>
Con el lease ledger: **0 / 0**, y las **96 ediciones se aplicaron** — ninguna rechazada, y 30 de ellas se desplazaron a un rango de líneas libre.<br>
[Ver las mediciones →](docs/conflict-zero.md)

Si Zaivern Code te resulta útil, una ⭐ **Star** ayuda a su desarrollo.

</div>

## Por qué Zaivern Code

Arrancar varios CLIs de programación con IA es fácil. Seguirles la pista no lo es. Cada
agente vive en su propia pestaña de terminal, pide aprobación a su propio ritmo y edita
archivos sin saber qué están haciendo los demás.

<!-- 出典: docs/conflict-zero.md §3.3 — 書き手 64 / 重なり 0.5:
     ベースラインは 57/64 のマージが衝突し 132 ハンク、ガード側は全規模で 0 ハンク -->

| Sin una cabina de mando | Con Zaivern Code |
|---|---|
| Más agentes en paralelo, más conflictos de merge | Un ledger compartido mantiene a cada agente fuera de las líneas de los demás — 0 hunks en conflicto con 64 agentes, donde git a secas produjo 132 |
| Recorrer pestañas para averiguar quién te necesita | Todos los agentes en una sola pantalla, con estado en vivo |
| Pegar la misma instrucción en cada herramienta | Difúndela una vez a toda la flota, o dirígete a un solo agente |
| Perder un aviso de aprobación y perder la ejecución | Notificaciones y aprobación con un clic |
| Quedarte en el escritorio mientras los agentes trabajan | Consulta el progreso y aprueba desde el móvil |

Zaivern Code no es un modelo de IA ni incluye ninguno. Conduce los CLIs que ya tienes
instalados y con la sesión iniciada — con uno basta para empezar.

## Inicio rápido

**Requisitos previos.** Instala e inicia sesión en al menos un CLI de programación con IA
compatible. Zaivern Code incluye presets de lanzamiento para 33 de ellos, entre ellos
Claude Code, Codex y Gemini CLI. No necesitas más de uno.

**macOS / Linux**

```bash
curl -fsSL https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.sh | sh
zai .
```

**Windows PowerShell**

```powershell
irm https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.ps1 | iex
zai .
```

Ambos instaladores verifican el archivo de la release contra el `checksums.txt`
publicado **antes de descomprimirlo**, y abortan sin extraer ni ejecutar nada si el
SHA-256 no coincide — o si los checksums no se pueden descargar en absoluto.

¿Prefieres no canalizar un script hacia tu shell? Descarga el archivo para tu plataforma
desde [Releases](https://github.com/tacyan/zaivern-code/releases/latest), descomprímelo y
coloca `zai` (o `zai.exe`) en algún lugar de tu `PATH`. Después ejecuta `zai .` en la
carpeta de un proyecto. En [SECURITY.md](SECURITY.md) se explica cómo verificar la
descarga a mano, comprobar la procedencia de la compilación o leer el SBOM.

Una vez abierta la ventana:

1. Pulsa `+ Agent` y elige un CLI que ya tengas instalado.
2. Escribe una tarea en el campo de entrada y envíala.
3. Añade un segundo agente cuando el primero te resulte cómodo.

### Actualizar

```bash
zai update            # busca una versión más reciente, muestra el comando y luego actualiza
zai update --check    # solo mira; no cambia nada
zai update --yes      # actualiza sin pedir confirmación
```

`zai update` funciona esté o no el editor en ejecución, y actualiza in situ a través del
script instalador de tu plataforma. Volver a ejecutar el one-liner de arriba hace lo
mismo.

`zai uninstall` lo elimina (`--dry-run` lista lo que se borraría). La desinstalación toca
únicamente el ejecutable y `~/.zaivern`; cualquier otra cosa en tu `PATH` se lista, nunca
se borra.

## Funcionalidades principales

### Coordinación de conflictos (la razón de que esto exista)

Los agentes reclaman los archivos — o los rangos de líneas concretos — que están a punto
de editar en un ledger compartido por repositorio, y los git hooks rechazan una escritura
que fuera a colisionar.

<!-- 出典: docs/conflict-zero.md §3.8.1 — --layout disjoint / 64 体:
     B (ファイル単位の所有) 完了 1・拒否 63、Cref (行域) 完了 64・拒否 0・ハンク 0 -->
Los rangos de líneas son lo que hace esto utilizable a escala. Apunta 64 agentes a un
único archivo y un lease a nivel de archivo deja pasar exactamente a **1** de ellos
mientras rechaza a los otros **63**; con propiedad por región de líneas pasan los **64**,
no se rechaza nada, y el merge sigue produciendo **0** hunks en conflicto.

<!-- 出典: docs/conflict-zero.md §3.12.2 — 錨の誤マッチによる二重配布と、その修正 -->
Una región se sigue mediante un ancla — el contenido de su primera y su última línea — en
lugar de por número de línea, así que sobrevive a las ediciones hechas por encima de ella.
Si al volver a resolver esa ancla se cae en un sitio distinto del que registró el ledger,
esa lectura se descarta en vez de darse por buena, de modo que una reserva nunca migra en
silencio a otra parte del archivo.

Nada de esto detecta un conflicto semántico; la [sección de abajo](#coordinación-de-conflictos)
detalla qué queda cubierto y qué no.

### Agent Cockpit

Coloca varios CLIs de IA en mosaico, uno al lado del otro, y ve de un vistazo cuál está
pensando, editando, ejecutando o esperándote. Vienen incorporados presets de lanzamiento
para 33 herramientas, así que añadir un agente es cuestión de dos clics en lugar de
recordar una línea de comandos.

### Broadcast

Envía una sola instrucción a todos los agentes en ejecución desde un único campo de
entrada, o elige un agente cuando quieras un control más concreto. Útil cuando la misma
corrección se aplica a toda la flota.

### Estado, aprobaciones y notificaciones

Zaivern Code expone los avisos de permisos, los atascos y las salidas inesperadas como
notificaciones sobre las que puedes actuar con un clic. La aprobación automática está
desactivada por defecto y hay que activarla de forma deliberada.

### Control remoto desde el móvil

Consulta el progreso, envía instrucciones, aprueba acciones y edita archivos desde tu
móvil. La configuración más sencilla funciona dentro de la misma red Wi-Fi. Cuando no
estés en ella, toma el relevo uno de estos dos transportes:
**[Tailscale](https://tailscale.com/)**, si ambas máquinas ya están en la misma tailnet, o
un túnel SSH a través de un host al que ya puedas llegar. Cambiar de transporte solo
cambia dónde escucha el servidor — el token, el puerto y la página siguen siendo los
mismos, así que un QR ya escaneado en el móvil sigue funcionando.

El modo Tailscale no necesita bastión ni redirección de puertos: instala
[Tailscale](https://tailscale.com/download) en el PC y en el móvil, inicia sesión en ambos
dentro de la misma tailnet y pulsa **🔒 Listen on Tailscale** en la ventana del control
remoto. Se enlaza a la dirección de la tailnet y a `127.0.0.1` y a nada más, así que la
Wi-Fi de la cafetería o del aeropuerto en la que estés no puede ver el puerto en absoluto.
Zaivern obtiene la dirección de la tailnet de la tabla de rutas del kernel y nunca invoca
el comando `tailscale` — en macOS ese CLI es un envoltorio de shell que puede quedarse
colgado para siempre cuando el daemon no es alcanzable, y un hijo colgado congelaría la
interfaz.

### Editor integrado

Lee código y revisa lo que cambiaron tus agentes sin salir de la aplicación, incluidos
imágenes, PDFs, CSVs y Markdown. Los búferes sin guardar sobreviven a un cierre
inesperado: el siguiente arranque los restaura y, si el archivo cambió en disco entretanto,
se te muestra la diferencia en lugar de sobrescribirlo en silencio.

## Coordinación de conflictos

Una reserva en el ledger no es un consejo: el hook rechaza la escritura que colisiona en
el momento en que se intenta, así que el choque aflora ahí en lugar de en el momento del
merge.

<!-- 出典: docs/conflict-zero.md §3.16.6 — dup_lines=0 は常に成立 (内容に依存しない)、
     conflict_files=0 は条件付き (帯 + 壁 + 昇順。反復的な内容では断ることがある) -->
Hay dos garantías que se cumplen en distinto grado, y mezclarlas exageraría el caso.
«A dos agentes nunca se les entregan las mismas líneas» es una propiedad del ledger y se
cumple independientemente de lo que contengan los archivos. «Y luego el merge pasa a la
primera» es condicional: necesita una banda de seguridad, una línea única entre las dos
regiones y orden ascendente. El contenido repetitivo puede romper la segunda mientras la
primera sigue en pie, y en ese caso la puerta rechaza en vez de adivinar.

Lo que no puede detectar es un conflicto semántico: un agente cambia la firma de una
función mientras otro sigue llamando a la antigua, en un archivo distinto, con un merge
perfectamente limpio.

```console
$ zai czero init      # instala el ledger, los git hooks y el merge driver, y luego se autodiagnostica
$ zai czero verify    # crea un conflicto real en un repositorio desechable y comprueba que se detiene
```

El alcance, los límites y las mediciones que hay detrás están en
[docs/conflict-zero.md](docs/conflict-zero.md).

## Uso de recursos

<!-- 出典: docs/idle-cost.md §7 — 2026-08-15、同一マシン・同一セッションで
     Zed 1.15.0 / zai 0.16.0 / zai 0.17.0 を交互に 3 ラウンド、9/9 VALID。
     0.16.0 を陽性対照に入れてあるので「測定が生きていること」まで示せる。
     0.17.0 は測定床に張り付いているので必ず「≤」で書くこと -->

Un editor que dejas abierto todo el día no debería costar nada mientras no estás
escribiendo. Medido en una misma máquina y en una misma sesión, alternando entre
aplicaciones tres veces (macOS 26.5.2, conectado a la corriente, ventanas de observación
de 180 segundos, un espacio de trabajo neutro de 4 archivos):

| | Zed 1.15.0 | Zaivern Code 0.17.0 |
|---|---:|---:|
| CPU en reposo (mediana de 3) | 0,761 % de un núcleo | **≤0,006 %** — en el suelo de medición |
| Descarga | 424,6 MB (`.app`) | **28,7 MB** (un solo binario) |
| RSS | 162,2 MB | 170,3 MB |

Dos cosas con las que esta tabla es cuidadosa:

- **`≤0,006 %` es un suelo, no una lectura.** `ps` resuelve el tiempo de CPU con una
  precisión de 1/100 s, así que una ventana de 180 segundos no puede distinguir nada por
  debajo de 0,006 %. Las tres rondas cayeron exactamente en un tick. La afirmación honesta
  es «al menos 127 veces más bajo que Zed», no una proporción.
- **El RSS no es una victoria, y no afirmamos que lo sea.** Zed también está escrito en
  Rust; los dos están a menos de un 5 % uno del otro, lo cual es ruido. El número que
  difiere en un orden de magnitud es el tamaño de la descarga.

La misma ejecución midió Zaivern Code **0.16.0** en 8,933 % — ese es el control positivo.
Sin una versión que produzca una lectura alta en la misma sesión, un resultado cercano a
cero no se puede distinguir de una medición estropeada. El coste en reposo bajó en 0.17.0
porque el tour guiado ya no reserva fotogramas de forma incondicional, y el repintado de
mantenimiento de cada dos segundos ha desaparecido.

Reprodúcelo con `tools/idle-duel.sh --vs Zed --out /tmp/duel.tsv`. El arnés se niega a
medir cuando no puede medir con honestidad: verifica por pid que la aplicación está en
primer plano, exige que la máquina esté sin tocar y registra la evidencia en cada fila. El
método completo, los números en bruto y las trampas con las que nos topamos están en
[docs/idle-cost.md](docs/idle-cost.md).

## Plataformas compatibles

| Elemento | Compatibilidad |
|---|---|
| SO | macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64 |
| CLIs de IA | 33 presets de lanzamiento, incluidos Claude Code, Codex y Gemini CLI |
| Rust | 1.88+ — solo al compilar desde el código fuente |
| Licencia | Apache-2.0 |

Una configuración habitual es Claude Code implementando, Codex probando y Gemini CLI
escribiendo documentación, pero nada en Zaivern Code da por supuesto ese reparto.
Cualquier combinación funciona, incluida la de un solo agente.

## Seguridad

- El modo con aprobación obligatoria es el predeterminado; el Auto-YES se activa
  explícitamente por sesión.
- La escalada de privilegios siempre requiere aprobación manual.
- Los valores de las variables de entorno de MCP nunca se muestran — solo si están
  definidas o no.
- Los procesos hijo se detienen cuando se destruye una sesión o la aplicación termina, así
  que ningún agente huérfano sigue ejecutándose en segundo plano.

## Preguntas frecuentes

**¿En qué se diferencia esto de tmux con paneles divididos?**

tmux coloca terminales en mosaico; no tiene ni idea de qué se está ejecutando dentro de
ellos. Zaivern Code lee el estado de cada agente, así que puede mostrar cuál está pensando,
editando o bloqueado en un aviso de aprobación, y convertir ese aviso en una notificación
que respondes con un clic. La parte para la que tmux no tiene equivalente es el ledger
compartido: dos agentes no pueden escribir físicamente en las mismas líneas, porque un git
hook rechaza la segunda escritura en el momento en que se intenta, en lugar de dejar que
se descubra en el merge.

**¿El lease ledger ralentiza las cosas?**

<!-- 出典: docs/conflict-zero.md §1「意味しないこと」4 / §3.3 (掃引: 4〜8 体 p50 40〜50ms、
     64 体 p50 160ms、busy-deny 32 体 4 件・64 体 14 件) / §3.4 (ゲート 1536 回で p50 298.7ms)。
     体数だけでは決まらないので、必ず担当表の大きさを添えること -->
Sí, y empeora con la escala, porque la puerta se sitúa en la ruta de escritura. En el
barrido estándar — N escritores sobre N×6 archivos — la latencia de la puerta es de p50
40–50 ms con 4–8 agentes y de p50 160 ms con 64. El número de agentes no es la única
variable: una tabla de asignación más pesada que llama a la puerta 1536 veces llega a p50
298,7 ms con esos mismos 64 agentes, así que cualquier cifra suelta del tipo «con 64
agentes cuesta X» queda incompleta sin el tamaño de la carga de trabajo. A partir de 32
agentes, la puerta además empieza a responder `busy-deny` cuando no puede decidir a
tiempo: rechaza en lugar de adivinar, y un reintento pasa, pero tú lo ves como un rechazo
ocasional. Con uno o dos agentes, la puerta no está en tu ruta crítica.

**¿Qué significa realmente «cero conflictos»?**

Algo más estrecho de lo que suena, y a propósito:

<!-- 出典: docs/conflict-zero.md §3.2 (書き手 8 / 重なり 1.00: 10/48 成立・38 件をゲートが停止)、
     §3.8.1 (disjoint / 64 体: 素の git のハンクは全規模 0。B は完了 1、Cref は 64)、§3.16.6 -->
- **El cero se compra rechazando escrituras.** Con ocho escritores apuntando todos a los
  mismos archivos, 10 de las 48 ediciones previstas se escribieron y las otras 38 se
  detuvieron en la puerta. El recuento de conflictos es 0; el rendimiento no.
- **Los rangos de líneas suficientemente separados nunca necesitaron ayuda.** git a secas
  ya los fusiona con cero conflictos. La propiedad por región de líneas no hace algo que
  git no pueda hacer — devuelve el paralelismo que destruye un lease a nivel de archivo
  (1 de 64 agentes pasa, frente a 64 de 64).
- **Las dos garantías no son igual de fuertes.** «A dos agentes nunca se les dan las
  mismas líneas» siempre se cumple; «el merge pasa a la primera» es condicional y puede
  fallar con contenido repetitivo.

[docs/conflict-zero.md](docs/conflict-zero.md) abre exactamente con esta frontera y
recoge todas las mediciones que hay detrás, incluidas las afirmaciones que después fueron
refutadas.

## Documentación

| Documento | De qué trata |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | Qué afirma «libre de conflictos», qué no, y las mediciones que lo respaldan |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | Qué garantías se cumplen para cada forma de repositorio |
| [docs/plugins.md](docs/plugins.md) | Cómo escribir plugins, con la [especificación del formato](docs/PLUGIN_SPEC.md) |
| [docs/README.md](docs/README.md) | Índice de todos los demás documentos, agrupados por la afirmación que respaldan |

Las notas de versión de cada release están en la
[página de Releases](https://github.com/tacyan/zaivern-code/releases).

## Contribuir

Los informes de errores, las peticiones de funcionalidades y los pull requests son
bienvenidos. Antes de abrir uno nuevo, revisa
[Issues](https://github.com/tacyan/zaivern-code/issues) por si ya existe el informe, y
abre un [Pull Request](https://github.com/tacyan/zaivern-code/pulls) contra `main`.

```bash
git clone https://github.com/tacyan/zaivern-code.git
cd zaivern-code
rustup update stable
cargo run --release -- .
```

[CONTRIBUTING.md](CONTRIBUTING.md) cubre el resto: cómo verificar un cambio, cómo ejecutar
las comprobaciones de Linux y Windows en local, y las convenciones que sigue este
repositorio.

## Licencia

[Apache License 2.0](LICENSE)

---

<div align="center">

**Los agentes ya son rápidos. Ahora te toca a ti mandar más rápido.**

</div>
