<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

### Ejecuta varios agentes de código sin el caos de los conflictos de fusión.

**Empieza con 2 agentes. Escala a 64.**
Zaivern Code detiene las ediciones solapadas antes de que se escriban, así que nunca
llegan a convertirse en conflictos de fusión.

Una sola ventana para Claude Code, Codex, Gemini CLI y otras 30 CLIs de agente que ya
tienes instaladas. Binario nativo único: macOS, Linux, Windows.

[English](README.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md) | [한국어](README.ko.md) | [Português (Brasil)](README.pt-BR.md) | **Español**

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

</div>

**Instalar y arrancar**

macOS / Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.sh | sh
zai .
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.ps1 | iex
zai .
```

Requiere al menos una CLI de código compatible ya instalada y con sesión iniciada.
Zaivern Code solo maneja las CLIs que ya tienes y no incluye ningún modelo de IA ni suscripción.

**Coordinación de conflictos (opcional):**

```bash
zai czero init
```

Esto modifica el repositorio Git actual.
[Previsualiza y verifica los cambios →](#activar-la-coordinación-de-conflictos) ·
[Descarga manual y verificación](SECURITY.md)

<div align="center">

<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="La cabina de Zaivern Code: varias CLIs de agente de código una junto a otra en una misma ventana, con el estado de cada agente" />
</a>

[**Inicio rápido**](#inicio-rápido) ·
[**Mediciones**](#mediciones-y-limitaciones) ·
[**Documentación**](#documentación) ·
[**Descarga**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**Sitio web**](https://zaivern.com/)

</div>

*El vídeo de arriba es la cabina: varias CLIs de agente en una ventana. No muestra la
coordinación de conflictos; eso se mide aparte, justo debajo.*

## Prueba

**64 agentes, un repositorio, la misma carga de trabajo.** Archivos = escritores × 6,
la mitad de ellos apuntados por más de un agente. La misma lista de tareas ejecutada dos
veces: una con git a secas y otra a través del registro de rangos de línea de Zaivern Code.

| | Git a secas | Zaivern Code |
|---|---:|---:|
| Fusiones con conflicto | 57 de 64 | **0 de 64** |
| Hunks de conflicto que quedan para una persona | 132 | **0** |
| Ediciones que entraron | 384 de 384 | 202 de 384 |
| Escrituras detenidas antes de entrar | 0 | 182 |

**El cero se compra rechazando escrituras, no fusionando ambos lados por arte de magia.**
182 de las 384 ediciones previstas se detuvieron en la puerta porque otro agente vivo ya
era dueño de esas líneas; 14 de las 182 fueron esperas por contención, que pueden pasar al reintentar.

**Cuando los rangos son de verdad disjuntos, no se rechaza nada.** 64 agentes editando
64 rangos separados de un *mismo* archivo colocan **64 de 64** ediciones, con **0**
rechazos y **0** hunks de conflicto, mientras que un bloqueo por archivo coloca 1 y rechaza 63.

Los conflictos semánticos **no** se detectan: si un agente cambia una firma y otro sigue
llamando a la antigua, ambas escrituras pasan y git las fusiona sin quejarse.

[Metodología, cifras por escala, latencia de la puerta y todas las lagunas abiertas →](docs/conflict-zero.md)

## El problema

Ejecutar un agente de código es fácil. Ejecutar cuatro no lo es. **Dos agentes editando el
mismo archivo ya bastan:**

- Editan las mismas líneas y te enteras al fusionar.
- No ves qué agente trabaja, cuál está bloqueado y cuál se ha parado en silencio.
- Una petición de aprobación pasa de largo en una pestaña que no estabas mirando.
- La integración acaba siendo tu trabajo, cada vez.

El cuello de botella no son los agentes, sino **la coordinación entre ellos**.

## La solución

Zaivern Code coordina qué partes del repositorio puede editar con seguridad cada agente.
En lugar de descubrir las colisiones al fusionar, detecta el trabajo solapado **antes de
que la escritura conflictiva entre**, y reúne en un solo sitio la forma de observar,
dirigir y recuperar los agentes que tienes en marcha.

```text
Sin Zaivern                              Con Zaivern

Agent 1  ─┐                              Agent 1  ─┐
Agent 2  ─┤                              Agent 2  ─┤   ┌─────────────┐
Agent 3  ─┼─→ mismos archivos ─→ fusión  Agent 3  ─┼─→ │ registro de │ ─→ integración
   ...   ─┤            con conflictos       ...   ─┤   │   líneas    │    limpia
Agent 64 ─┘                              Agent 64 ─┘   └─────────────┘
```

## Inicio rápido

### Arrancar la cabina multiagente

Instala con la orden de una línea que hay al principio de esta página y ejecuta `zai .` en
la carpeta de un proyecto. La cabina se abre sobre esa carpeta: paneles de agente, editor y
control desde el móvil. Pulsa `+ Agent`, elige una CLI que tengas instalada y mándale una tarea.
**Esto no activa la coordinación de conflictos**; ese es el paso siguiente.

Los instaladores comprueban el archivo descargado contra el `checksums.txt` de la release
**antes de descomprimir**, y abortan si no coincide.
[Descarga manual, verificación de checksum, procedencia y SBOM →](SECURITY.md)

### Activar la coordinación de conflictos

```bash
zai czero init --dry-run  # previsualiza los cambios previstos
zai czero init            # instala el registro y la integración con Git
zai czero verify          # verifícalo en repositorios desechables
zai .                     # arranca la cabina
```

- **`zai czero init --dry-run`** previsualiza los cambios previstos sin modificar el
  repositorio actual.
- **`zai czero init` modifica el repositorio Git actual.** Prepara el registro de rangos de
  línea, añade los hooks `pre-commit` / `pre-applypatch` / `pre-merge-commit`, registra el
  union merge driver y escribe un bloque gestionado en `.gitattributes`; después se
  autodiagnostica. Es idempotente.
- **`zai czero verify`** crea escrituras solapadas reales y fusiones reales en repositorios
  desechables y comprueba que cada una se detiene de verdad. **No modifica el repositorio
  actual.** El veredicto es `verified` / `partial` / `broken`: no dirá "verified" si hubo
  alguna prueba que no pudo ejecutar.
- **`zai czero doctor`** diagnostica qué capas siguen activas, y **`zai czero uninstall`**
  quita exactamente lo que añadió `init`.

### Actualización

`zai update` muestra la orden que va a ejecutar y luego actualiza (`--check` solo consulta,
`--yes` se salta la confirmación). Funciona con el editor abierto o cerrado.
`zai uninstall` lo desinstala.

## Funciones principales

Ordenadas por cuánto diferencian a Zaivern Code. La primera es la razón de que exista.

### 1. Propiedad de archivos y rangos de línea, aplicada al escribir

Los agentes reclaman archivos o rangos de línea antes de editar, anclados al **contenido
que los rodea** y no al número de línea. Si otro agente vivo ya posee una región solapada,
un hook de git rechaza la escritura: al escribir, no al fusionar. El mismo archivo con
líneas distintas sí se permite, y eso es lo que mantiene a los agentes en paralelo en vez
de serializarlos detrás de un bloqueo de archivo entero.
[Cómo funciona la coordinación por rango de línea →](docs/conflict-zero.md)

### 2. Una pantalla, y ves qué hace cada agente

Coloca varias CLIs de IA una junto a otra y comprueba de un vistazo cuál está pensando,
editando, ejecutando o esperándote. Añadir un agente son dos clics, no una línea de
comandos que hay que recordar.

### 3. Detección de bloqueo y de salida

Zaivern Code observa el progreso semántico, no los píxeles: un agente que deja de avanzar
se informa como **bloqueado**, y las salidas inesperadas aparecen como notificaciones.

### 4. Instrucción masiva y dirigida

Envía una misma instrucción a todos los agentes en marcha desde un único campo, o apunta a
uno solo cuando quieras control preciso.

### 5. Aprobaciones

El modo con aprobación obligatoria es el predeterminado. El auto-YES se activa por sesión,
la elevación de privilegios siempre necesita a una persona y los valores de las variables
de entorno de MCP no se muestran nunca.

### 6. Control desde el móvil

Consulta el progreso, envía instrucciones, aprueba acciones y edita archivos desde el
móvil. Usa la misma Wi-Fi, [Tailscale](https://tailscale.com/) o un túnel SSH.

### 7. Editor integrado

Revisa el código y los cambios de los agentes sin salir de Zaivern Code, incluidos
Markdown, imágenes, PDF y CSV. Los búferes sin guardar se recuperan tras un fallo.

### 8. Ejecuciones de equipo con IA — entrega un SPEC y obtén un equipo gestionado

```sh
zai team run SPEC.md --agents 4
```

Zaivern lee el SPEC, deriva un Goal y una Definition of Done, construye un grafo
de tareas y muestra el plan. Al pulsar **Start Team**, arranca solo los agentes
que el plan realmente necesita, entrega a cada uno su tarea y lleva el trabajo
de implementar → validar → revisar → corregir → integrar hasta el final.

**Nada se marca como completado porque un agente lo diga.** Una tarea solo
avanza por `Running → Validating → Reviewing → Completed`, y un informe de
finalización se rechaza si el id de la tarea o del agente no coincide, si tocó
archivos fuera de su alcance, si los comandos de validación no se ejecutaron o
fallaron, o si queda un blocker abierto. Las revisiones van a una **sesión
distinta** de la que escribió el código. El bloque `validation` que informa el
agente se guarda solo como **información de referencia**: es Zaivern quien
ejecuta los comandos de validación y solo pasa a la revisión con los resultados
que él mismo midió.

Los comandos de validación se **clasifican por riesgo**, no se dan por buenos
por estar en una allowlist. Un ejecutable con ruta (`/tmp/cargo test`,
`./cargo test`, `tools/python x.py`) nunca se ejecuta: mirar solo el basename
ejecutaría lo que sea que sea `/tmp/cargo`. push, merge, deploy, publish, la
elevación de privilegios y los comandos destructivos se rechazan. Y todo lo que
puede ejecutar código del repositorio (`cargo test`, `npm test`, `pytest`,
`make`, `node`, `go test`) **espera tu aprobación antes de ejecutar una sola
línea**: el cuerpo de un test, un `build.rs` o un `Makefile` pueden hacer lo
mismo que un shell. Cada ejecución tiene tiempo límite, se termina con todo el
árbol de procesos cuando paras el equipo, y siempre acaba en un resultado:
pasó, falló, se agotó el tiempo, se canceló, no pudo iniciarse o se perdió la
conexión con el ejecutor. Zaivern no aísla lo que apruebas: garantiza **qué se
inició**, no lo que ese proceso hace después. `push`, `merge`, `deploy`, la elevación
de privilegios y los comandos destructivos nunca se ejecutan automáticamente:
se convierten en una decisión tuya, en pantalla.

El Organization Board muestra al líder del equipo, los carriles de cada
especialidad, todos los agentes padres e hijos, qué está haciendo cada uno ahora
mismo, el progreso del grafo de tareas, los resultados de pruebas y revisiones y
**lo que más necesita tu atención**.

[Documentación del equipo de IA](docs/team.md)

También se incluyen: plugins y una interfaz en seis idiomas.
[Documentación de plugins](docs/plugins.md) · [Documentación de traducción](docs/translating.md)

## Cómo funciona

1. **Arranca** los agentes de código desde una ventana, o engancha los que ya tienes en marcha.
2. **Reclama** archivos o rangos de línea antes de editar, anclados al contenido que los rodea.
3. **Barrera**: un hook de git rechaza una escritura solapada antes de que llegue a la fusión.
4. **Integra**: los cambios que no se solapan se fusionan por git como siempre.

## Agentes compatibles

Claude Code · Codex · Gemini CLI · Cursor Agent · GitHub Copilot CLI ·
**28 más**: 33 preajustes de arranque en total, además de 6 agentes manejables por ACP.

Vale cualquier combinación, incluido un solo agente.
¿Falta el tuyo? [Pide una integración](https://github.com/tacyan/zaivern-code/issues).

## Por qué Zaivern

|  | Multiplexor de terminal | Panel genérico de agentes | Zaivern Code |
|---|:---:|:---:|:---:|
| Propiedad por rango de línea + rechazo al escribir | ❌ | ❌ | ✅ |
| Conoce el estado del agente (pensando / bloqueado / parado) | ❌ | varía | ✅ |
| Una pantalla para todos los agentes a la vez | ❌ | ✅ | ✅ |
| Aprobaciones como notificaciones | ❌ | varía | ✅ |
| Control desde el móvil / remoto | ❌ | varía | ✅ |
| Binario nativo único, sin runtime | varía | varía | ✅ |

## Mediciones y limitaciones

La tabla de 64 agentes de arriba es sintética. En **repositorios reales**, clonados y
reproducidos por `tools/anyrepo-prove.sh` con 16 escritores (zai 0.14.0):

| Repositorio | Git a secas | Zaivern Code |
|---|---|---|
| zaivern-code (Rust, 259 archivos rastreados) | 26 archivos en conflicto / 28 hunks | **0 / 0**: 96 de 96 ediciones entraron, 0 rechazos, 30 desplazadas |
| hyperframes (TS/HTML, 1194 archivos rastreados) | 26 / 28 | **0 / 0**: 96 de 96 entraron, 0 rechazos, 32 desplazadas |

Rechazar no es el único desenlace. Cuando una reclamación choca, `--shift` la mueve al
rango libre más cercano que admita el mismo ancho; por eso las dos filas de arriba colocan
todas las ediciones y no rechazan ninguna.

### Qué significa "cero conflictos"

- **La propiedad se cumple siempre.** "A dos agentes nunca se les dan las mismas líneas"
  depende solo del registro, no del contenido del archivo: `dup_lines = 0` en 126 de 126
  ejecuciones independientes de la prueba.
- **Una fusión limpia es condicional.** En contenido repetitivo (vallas de código
  repetidas, código generado, la misma línea una y otra vez) git todavía puede dar
  conflicto aunque los rangos reclamados estén suficientemente separados. La puerta
  **rechaza esas reclamaciones** en vez de prometer una fusión que no puede garantizar.
- **Los conflictos semánticos quedan fuera.** Lo que se impide es el solapamiento de
  propiedad de líneas; una firma cambiada y una llamada antigua en otro archivo, no.
- **El trabajo disjunto nunca necesitó ayuda.** Los rangos suficientemente separados ya se
  fusionan con cero conflictos en git a secas. La propiedad por rango de línea devuelve el
  **paralelismo que destruye un bloqueo por archivo**: esa es la comparación que importa.
- **Solo se aplica donde git puede aplicarlo.** `zai lease claim` también tiene éxito en
  una carpeta que no es git, pero allí no se detiene nada. `zai czero doctor` informa de
  qué formas de repositorio (worktrees, submódulos, sparse-checkout, LFS, bare) quedan
  realmente cubiertas.

Reproduce cualquiera de estos datos: `tools/conflict-bench.sh`, `tools/coedit-bench.sh`,
`tools/anyrepo-prove.sh --repo .`
[Metodología completa y lagunas pendientes →](docs/conflict-zero.md) ·
[qué garantías valen para cada forma de repositorio →](docs/czero-repo-shapes.md)

## Plataformas compatibles

| Elemento | Compatibilidad |
|---|---|
| SO | macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64 |
| Distribución | Binario nativo único, sin runtime; checksums, SBOM y procedencia de compilación en cada release |
| CLIs de IA | 33 preajustes de arranque, más 6 por ACP |
| Pruebas | 5005 en la v0.23.0, ejecutadas en macOS, Linux y Windows en CI |
| Licencia | Apache-2.0 |

## Documentación

| Documento | Qué cubre |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | Qué afirma "sin conflictos", qué no afirma y cada medición que lo respalda |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | Qué garantías valen para cada forma de repositorio |
| [docs/idle-cost.md](docs/idle-cost.md) | Cómo se miden la CPU en reposo y el tamaño del binario |
| [docs/plugins.md](docs/plugins.md) | Cómo escribir plugins, con la [especificación del formato](docs/PLUGIN_SPEC.md) |
| [docs/team.md](docs/team.md) | `zai team`: cómo un SPEC se convierte en un grafo de tareas, qué habilita el "completado" y qué nunca se ejecuta automáticamente |
| [docs/README.md](docs/README.md) | Índice de todos los demás documentos, agrupados por la afirmación que respaldan |

[Notas de la versión](https://github.com/tacyan/zaivern-code/releases) ·
[Política de seguridad](SECURITY.md) · [Cómo contribuir](CONTRIBUTING.md)

## Pruébalo

Prueba Zaivern Code con dos agentes en el mismo repositorio:

```bash
zai czero init
zai .
```

Arranca dos agentes, apunta ambos al mismo archivo y observa cómo la segunda escritura
solapada se rechaza *antes* de convertirse en un conflicto de fusión. Esa es la idea
entera, en cosa de un minuto.

Si te resulta útil, una ⭐ **Star** ayuda a que otras personas lo encuentren.

## Comunidad

- ¿Has encontrado un caso límite de coordinación? [Abre una issue](https://github.com/tacyan/zaivern-code/issues).
- ¿Usas un agente de código que aún no es compatible? [Pide una integración](https://github.com/tacyan/zaivern-code/issues).
- ¿Ejecutas 8, 16, 32 o 64 agentes? Comparte tus cifras: `tools/conflict-bench.sh` y
  `tools/anyrepo-prove.sh` producen resultados comparables con las tablas de arriba.

Los pull requests son bienvenidos contra `main`: [CONTRIBUTING.md](CONTRIBUTING.md) explica
cómo compilar desde el código fuente (Rust 1.88+), cómo verificar un cambio y cómo ejecutar
las comprobaciones de Linux y Windows en local.

## Licencia

[Apache License 2.0](LICENSE)
