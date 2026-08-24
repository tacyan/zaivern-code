<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

### 64 agentes de IA. Un repositorio. Cero conflictos de fusión.

**La capa de coordinación para agentes de código en paralelo.**

Ejecuta Claude Code, Codex, Gemini CLI y otros agentes de código sobre el mismo
repositorio — sin el caos de los conflictos de fusión.

[English](README.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md) | [한국어](README.ko.md) | [Português (Brasil)](README.pt-BR.md) | **Español**

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

<!-- TODO: Reemplazar por una demo de 15-20 s del benchmark:
     64 agentes en un repositorio / git puro 132 hunks de conflicto / Zaivern 0.
     El GIF de abajo muestra la cabina, no el resultado de la coordinación. -->
<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Zaivern Code ejecutando Claude Code, Codex, Gemini CLI y otros agentes de código en paralelo" />
</a>

| 64 escritores · mismo repositorio · misma carga | Git puro | Zaivern Code |
|---|---:|---:|
| Fusiones con conflicto | 57 de 64 | **0** |
| Hunks de conflicto | 132 | **0** |

[Consulta la metodología, las concesiones y los límites →](docs/conflict-zero.md)

[**Inicio rápido**](#inicio-rápido) ·
[**Mediciones**](#mediciones) ·
[**Documentación**](#documentación) ·
[**Descarga**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**Sitio web**](https://zaivern.com/)

</div>

## El problema

Ejecutar un agente de código es fácil. Ejecutar cuatro, no. Con dos agentes editando el
mismo archivo ya basta:

- Editan las mismas líneas y te enteras al fusionar.
- No ves cuál agente trabaja, cuál está bloqueado y cuál se detuvo en silencio.
- Una petición de aprobación pasa en una pestaña que no estabas mirando.
- La integración se convierte en tu trabajo — cada vez.

El cuello de botella no son los agentes, sino **la coordinación entre ellos**.

## La solución

Zaivern Code coordina qué partes del repositorio puede editar con seguridad cada agente.
En lugar de descubrir las colisiones al fusionar, detiene el trabajo solapado **antes de
que la escritura conflictiva se materialice** — y reúne en un solo sitio la
observación, el control y la recuperación de los agentes en marcha.

```text
Sin Zaivern                              Con Zaivern

Agente 1  ─┐                             Agente 1  ─┐
Agente 2  ─┤                             Agente 2  ─┤   ┌──────────────┐
Agente 3  ─┼─→ mismos    ─→ conflictos   Agente 3  ─┼─→ │ registro de  │ ─→ integración
   ...    ─┤    archivos    de fusión       ...    ─┤   │ rangos de    │    limpia
Agente 64 ─┘                             Agente 64 ─┘   │ líneas       │
                                                        └──────────────┘
132 hunks de conflicto                   0 hunks de conflicto
```

**No hacen falta 64 agentes para que esto importe.** Dos agentes editando el mismo
archivo bastan. Empieza con 2, escala a 64.

## Inicio rápido

Primero instala e inicia sesión en al menos una CLI de código con IA compatible —
Zaivern Code incluye **33** preajustes de lanzamiento, y con uno basta para empezar.

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

En la ventana: pulsa `+ Agent`, elige una CLI que ya tengas instalada y mándale una tarea.

Activa la coordinación de conflictos en un repositorio:

```bash
zai czero init      # instala el registro, los hooks de git y el merge driver, y se autodiagnostica
zai czero verify    # crea un conflicto real en un repositorio desechable y comprueba que se detiene
```

Los instaladores verifican el archivo contra el `checksums.txt` publicado **antes de
descomprimirlo** y abortan si no coincide.
[Descarga manual, verificación de checksum, procedencia y SBOM →](SECURITY.md)

### Actualización

```bash
zai update            # busca una versión nueva, muestra el comando y actualiza
zai update --check    # solo comprueba; no cambia nada
zai update --yes      # actualiza sin pedir confirmación
```

Funciona con el editor abierto o cerrado. Para desinstalar, `zai uninstall`.

## Funciones principales

### 1. Agentes en paralelo sin el caos de los conflictos de fusión

Los agentes reservan archivos o rangos de líneas antes de editar. Si otro agente activo
ya posee esa región, un hook de git rechaza la escritura conflictiva — en el momento de
escribir, no al fusionar.

En el benchmark de 64 agentes con rangos disjuntos, los **64** escribieron sus cambios
con **0** hunks de conflicto, donde un lease por archivo habría dejado pasar exactamente 1.
[Cómo funciona la coordinación por rango de líneas →](docs/conflict-zero.md)

### 2. Gestión de agentes en paralelo

Coloca varias CLIs en paralelo y ve de un vistazo cuál está pensando, editando,
ejecutando o esperándote. Añadir un agente son dos clics, no un comando que recordar.

### 3. Estado del agente y detección de bloqueo

Zaivern observa el progreso semántico, no los píxeles: un agente que deja de avanzar se
reporta como **bloqueado**, y las salidas inesperadas aparecen como notificaciones.

### 4. Instrucción masiva

Envía una misma instrucción a todos los agentes en marcha desde un único campo, o dirígete
a uno solo cuando quieras un control puntual.

### 5. Aprobaciones

El modo con aprobación obligatoria es el predeterminado. El Auto-YES se activa por sesión,
la elevación de privilegios siempre la confirma una persona y los valores de las variables
de entorno de MCP nunca se muestran.

### 6. Control desde el móvil

Consulta el progreso, envía instrucciones, aprueba acciones y edita archivos desde el
móvil. Con la misma Wi-Fi, con [Tailscale](https://tailscale.com/) o por un túnel SSH.

### 7. Editor integrado

Revisa el código y lo que cambiaron los agentes sin salir de Zaivern, incluidos Markdown,
imágenes, PDF y CSV. Los búferes sin guardar se recuperan tras un fallo.

También incluye: plugins y una interfaz disponible en seis idiomas.
[Documentación de plugins](docs/plugins.md) · [Documentación de traducción](docs/translating.md)

## Cómo funciona

1. **Lanza** los agentes desde una sola ventana, o conéctate a los que ya ejecutas.
2. **Reserva** archivos o rangos de líneas antes de editar, anclados al contenido contiguo.
3. **Bloquea** — un hook de git rechaza la escritura solapada antes de que llegue a la fusión.
4. **Integra** — los cambios que no se solapan se fusionan con git como siempre.

[Detalles técnicos →](docs/conflict-zero.md) ·
[qué garantías valen para cada forma de repositorio →](docs/czero-repo-shapes.md)

## Agentes compatibles

Claude Code · Codex · Gemini CLI · Cursor Agent · GitHub Copilot CLI ·
**28 más** — 33 preajustes de lanzamiento en total, más 6 agentes manejables por ACP.

Zaivern Code no es un modelo de IA ni incluye ninguno: solo maneja las CLIs que ya tienes
instaladas y con sesión iniciada. Cualquier combinación sirve, incluso un único agente.
¿Falta la tuya? [Pide una integración](https://github.com/tacyan/zaivern-code/issues).

## Por qué Zaivern

|  | Multiplexor de terminal | Panel genérico de agentes | Zaivern Code |
|---|:---:|:---:|:---:|
| Ejecutar varios agentes a la vez | ✅ | ✅ | ✅ |
| Una sola pantalla para todos | ❌ | ✅ | ✅ |
| Conoce el estado (pensando / bloqueado / detenido) | ❌ | varía | ✅ |
| Posesión de rangos de líneas + rechazo al escribir | ❌ | ❌ | ✅ |
| Aprobaciones como notificaciones | ❌ | varía | ✅ |
| Control móvil / remoto | ❌ | varía | ✅ |
| Un solo binario nativo, sin runtime | varía | varía | ✅ |

## Mediciones

**64 agentes, un repositorio, misma carga** (archivos = escritores × 6, 50% de
solapamiento de archivos):

| | Git puro | Zaivern Code |
|---|---:|---:|
| Fusiones con conflicto | 57 de 64 | **0** |
| Hunks de conflicto | 132 | **0** |

El cero se paga rechazando escrituras: de 384 ediciones planificadas entraron 202 y el
resto se detuvo en la puerta. Cuando los rangos de líneas son realmente disjuntos, los 64
agentes escriben y no se rechaza ninguna.

**Este repositorio, 16 agentes en paralelo** (zai 0.14.0): git puro produjo **26 archivos
en conflicto / 28 hunks**. Con el registro: **0 / 0**, y las **96 ediciones entraron** —
ninguna rechazada, 30 de ellas desplazadas a un rango de líneas libre.

### Qué significa "cero conflictos"

- Zaivern puede **rechazar** una escritura solapada en lugar de dejar que se convierta en
  un conflicto de fusión. El número de conflictos es 0; el rendimiento no.
- Evita el solapamiento en la posesión de líneas. **No** detecta conflictos semánticos:
  un agente cambia una firma, otro sigue llamando a la antigua y la fusión sale limpia.
- Los rangos de líneas lo bastante separados nunca necesitaron ayuda: git puro ya los
  fusiona sin conflictos. La posesión por rango devuelve el paralelismo que destruye un
  lease por archivo.

[Metodología completa, cifras por escala, latencia de la puerta y límites →](docs/conflict-zero.md)

## Plataformas compatibles

| Elemento | Soporte |
|---|---|
| SO | macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64 |
| CLIs de IA | 33 preajustes de lanzamiento, más 6 vía ACP |
| Pruebas | 4.985, ejecutadas en macOS, Linux y Windows en CI |
| Licencia | Apache-2.0 |

## Documentación

| Documento | Qué cubre |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | Qué afirma "sin conflictos", qué no afirma y cada medición detrás de ello |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | Qué garantías valen para cada forma de repositorio |
| [docs/plugins.md](docs/plugins.md) | Cómo escribir plugins, con la [especificación del formato](docs/PLUGIN_SPEC.md) |
| [docs/README.md](docs/README.md) | Índice del resto de documentos, agrupados por la afirmación que respaldan |

[Mediciones de CPU en reposo y tamaño del binario →](docs/idle-cost.md) ·
[Notas de versión](https://github.com/tacyan/zaivern-code/releases)

## Pruébalo

Si los agentes de código en paralelo forman parte de tu trabajo, ejecuta Zaivern Code en
tu próxima tarea multiagente — `zai czero init` en el repositorio, luego pon dos agentes
sobre el mismo archivo y observa cómo la segunda escritura se rechaza en vez de acabar en
una mala fusión.

## Comunidad

- ¿Encontraste un caso límite de coordinación? [Abre una issue](https://github.com/tacyan/zaivern-code/issues).
- ¿Usas un agente de código aún no compatible? [Pide una integración](https://github.com/tacyan/zaivern-code/issues).
- ¿Ejecutas 8, 16, 32 o 64 agentes? Comparte tu medición — `tools/conflict-bench.sh` y
  `tools/anyrepo-prove.sh` producen cifras comparables con las tablas de arriba.
- ¿Construiste algo con Zaivern Code? Enséñanos tu configuración.

Los pull requests son bienvenidos contra `main` — [CONTRIBUTING.md](CONTRIBUTING.md)
explica cómo compilar desde el código (Rust 1.88+), cómo verificar un cambio y cómo
ejecutar localmente las comprobaciones de Linux y Windows.

Si Zaivern Code te resulta útil, una ⭐ **Star** ayuda a que otras personas lo encuentren.

## Licencia

[Apache License 2.0](LICENSE)
