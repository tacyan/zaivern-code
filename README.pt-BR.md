<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

### 64 agentes de IA. Um repositório. Zero conflitos de merge.

**A camada de coordenação para agentes de código em paralelo.**

Rode Claude Code, Codex, Gemini CLI e outros agentes de código no mesmo repositório —
sem o caos dos conflitos de merge.

[English](README.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md) | [한국어](README.ko.md) | **Português (Brasil)** | [Español](README.es.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

<!-- TODO: Substituir por uma demo de 15-20 s do benchmark:
     64 agentes em um repositório / git puro 132 hunks de conflito / Zaivern 0.
     O GIF abaixo mostra o cockpit, não o resultado da coordenação. -->
<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Zaivern Code rodando Claude Code, Codex, Gemini CLI e outros agentes de código lado a lado" />
</a>

| 64 escritores · mesmo repositório · mesma carga | Git puro | Zaivern Code |
|---|---:|---:|
| Merges que conflitaram | 57 de 64 | **0** |
| Hunks de conflito | 132 | **0** |

[Veja a metodologia, os trade-offs e os limites →](docs/conflict-zero.md)

[**Início rápido**](#início-rápido) ·
[**Benchmarks**](#benchmarks) ·
[**Documentação**](#documentação) ·
[**Download**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**Site**](https://zaivern.com/)

</div>

## O problema

Rodar um agente de código é fácil. Rodar quatro, não. Dois agentes editando o mesmo
arquivo já bastam:

- Eles editam as mesmas linhas, e você descobre isso na hora do merge.
- Não dá para ver qual agente está trabalhando, bloqueado ou parado em silêncio.
- Um pedido de aprovação passa numa aba que você não estava olhando.
- A integração vira trabalho seu — toda vez.

O gargalo não são os agentes. É a **coordenação entre eles**.

## A solução

O Zaivern Code coordena quais partes do repositório cada agente pode editar com
segurança. Em vez de descobrir colisões no merge, ele intercepta o trabalho sobreposto
**antes que a escrita conflitante aconteça** — e reúne num só lugar o acompanhamento,
o controle e a recuperação dos agentes em execução.

```text
Sem o Zaivern                            Com o Zaivern

Agente 1  ─┐                             Agente 1  ─┐
Agente 2  ─┤                             Agente 2  ─┤   ┌──────────────┐
Agente 3  ─┼─→ mesmos     ─→ conflitos   Agente 3  ─┼─→ │ registro de  │ ─→ integração
   ...    ─┤    arquivos     de merge       ...    ─┤   │ faixas de    │    limpa
Agente 64 ─┘                             Agente 64 ─┘   │ linhas       │
                                                        └──────────────┘
132 hunks de conflito                    0 hunks de conflito
```

**Você não precisa de 64 agentes para isso importar.** Dois agentes editando o mesmo
arquivo bastam. Comece com 2, escale até 64.

## Início rápido

Antes, instale e faça login em pelo menos uma CLI de código com IA compatível — o
Zaivern Code traz **33** presets de execução, e um já é suficiente para começar.

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

Na janela: clique em `+ Agent`, escolha uma CLI que você já tenha instalada e mande uma
tarefa.

Ative a coordenação de conflitos em um repositório:

```bash
zai czero init      # instala o registro, os hooks do git e o merge driver, e se autodiagnostica
zai czero verify    # cria um conflito real num repositório descartável e confere que ele é barrado
```

Os instaladores verificam o arquivo contra o `checksums.txt` publicado **antes de
descompactar** e abortam se não bater.
[Download manual, verificação de checksum, proveniência e SBOM →](SECURITY.md)

### Atualização

```bash
zai update            # procura uma versão nova, mostra o comando e atualiza
zai update --check    # só verifica; não muda nada
zai update --yes      # atualiza sem pedir confirmação
```

Funciona com o editor aberto ou fechado. Para remover, `zai uninstall`.

## Principais recursos

### 1. Agentes em paralelo sem o caos dos conflitos de merge

Os agentes reservam arquivos ou faixas de linhas antes de editar. Se outro agente ativo
já é dono daquela região, um hook do git recusa a escrita conflitante — na hora da
escrita, não no merge.

No benchmark de 64 agentes com faixas disjuntas, todos os **64** gravaram suas edições
com **0** hunks de conflito, onde um lease por arquivo teria deixado passar exatamente 1.
[Como funciona a coordenação por faixa de linhas →](docs/conflict-zero.md)

### 2. Gerenciamento de agentes em paralelo

Coloque várias CLIs lado a lado e veja num relance qual está pensando, editando,
executando ou esperando por você. Adicionar um agente são dois cliques, não um comando
para lembrar.

### 3. Saúde do agente e detecção de travamento

O Zaivern observa progresso semântico, não pixels: um agente que para de progredir é
reportado como **travado**, e saídas inesperadas viram notificações.

### 4. Instrução em massa

Envie uma instrução para todos os agentes em execução a partir de um único campo, ou
escolha um agente quando quiser controle focado.

### 5. Aprovações

O modo com aprovação obrigatória é o padrão. O Auto-YES é opcional por sessão, elevação
de privilégio sempre passa por uma pessoa, e valores de variáveis de ambiente do MCP
nunca são exibidos.

### 6. Controle pelo celular

Acompanhe o progresso, envie instruções, aprove ações e edite arquivos pelo celular.
Use o mesmo Wi-Fi, [Tailscale](https://tailscale.com/) ou um túnel SSH.

### 7. Editor embutido

Revise código e o que os agentes mudaram sem sair do Zaivern, incluindo Markdown,
imagens, PDFs e CSVs. Buffers não salvos são recuperados depois de um crash.

Também incluídos: plugins e uma interface disponível em seis idiomas.
[Documentação de plugins](docs/plugins.md) · [Documentação de tradução](docs/translating.md)

## Como funciona

1. **Inicie** os agentes a partir de uma janela, ou conecte-se aos que já estão rodando.
2. **Reserve** arquivos ou faixas de linhas antes de editar, ancorados ao conteúdo ao redor.
3. **Barre** — um hook do git recusa a escrita sobreposta antes que ela chegue ao merge.
4. **Integre** — mudanças que não se sobrepõem entram pelo merge normal do git.

[Detalhes técnicos →](docs/conflict-zero.md) ·
[quais garantias valem para qual formato de repositório →](docs/czero-repo-shapes.md)

## Agentes compatíveis

Claude Code · Codex · Gemini CLI · Cursor Agent · GitHub Copilot CLI ·
**mais 28** — 33 presets de execução no total, além de 6 agentes acionáveis via ACP.

O Zaivern Code não é um modelo de IA e não embute nenhum: ele apenas dirige as CLIs que
você já instalou e nas quais já fez login. Qualquer combinação funciona, inclusive um
único agente. Falta a sua?
[Peça uma integração](https://github.com/tacyan/zaivern-code/issues).

## Por que o Zaivern

|  | Multiplexador de terminal | Painel genérico de agentes | Zaivern Code |
|---|:---:|:---:|:---:|
| Rodar vários agentes ao mesmo tempo | ✅ | ✅ | ✅ |
| Uma tela para todos eles | ❌ | ✅ | ✅ |
| Sabe o estado (pensando / bloqueado / travado) | ❌ | varia | ✅ |
| Posse de faixa de linhas + recusa na escrita | ❌ | ❌ | ✅ |
| Aprovações como notificações | ❌ | varia | ✅ |
| Controle por celular / remoto | ❌ | varia | ✅ |
| Binário nativo único, sem runtime | varia | varia | ✅ |

## Benchmarks

**64 agentes, um repositório, mesma carga** (arquivos = escritores × 6, 50% de
sobreposição de arquivos):

| | Git puro | Zaivern Code |
|---|---:|---:|
| Merges que conflitaram | 57 de 64 | **0** |
| Hunks de conflito | 132 | **0** |

O zero é pago recusando escritas: 202 das 384 edições planejadas entraram, o resto foi
barrado no portão. Quando as faixas de linhas são de fato disjuntas, todos os 64 agentes
gravam e nada é recusado.

**Este repositório, 16 agentes em paralelo** (zai 0.14.0): o git puro produziu
**26 arquivos em conflito / 28 hunks**. Com o registro: **0 / 0**, e todas as **96
edições entraram** — nenhuma recusada, 30 delas deslocadas para uma faixa livre.

### O que "zero conflitos" quer dizer

- O Zaivern pode **recusar** uma escrita sobreposta em vez de deixá-la virar um conflito
  de merge. A contagem de conflitos é 0; a vazão não é.
- Ele evita a sobreposição de posse de linhas. Ele **não** detecta conflitos semânticos —
  um agente muda uma assinatura, outro continua chamando a antiga, e o merge sai limpo.
- Faixas de linhas suficientemente distantes nunca precisaram de ajuda: o git puro já as
  junta com zero conflitos. A posse por faixa devolve o paralelismo que um lease por
  arquivo destrói.

[Metodologia completa, números por escala, latência do portão e limites →](docs/conflict-zero.md)

## Plataformas compatíveis

| Item | Suporte |
|---|---|
| SO | macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64 |
| CLIs de IA | 33 presets de execução, mais 6 via ACP |
| Testes | 4.985, executados em macOS, Linux e Windows na CI |
| Licença | Apache-2.0 |

## Documentação

| Documento | O que cobre |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | O que "livre de conflitos" afirma, o que não afirma, e cada medição por trás disso |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | Quais garantias valem para qual formato de repositório |
| [docs/plugins.md](docs/plugins.md) | Como escrever plugins, com a [especificação do formato](docs/PLUGIN_SPEC.md) |
| [docs/README.md](docs/README.md) | Índice de todos os outros documentos, agrupados pela afirmação que sustentam |

[Medições de CPU ociosa e tamanho do binário →](docs/idle-cost.md) ·
[Notas de versão](https://github.com/tacyan/zaivern-code/releases)

## Experimente

Se agentes de código em paralelo fazem parte do seu dia a dia, rode o Zaivern Code na
próxima tarefa multiagente — `zai czero init` no repositório, depois coloque dois agentes
no mesmo arquivo e veja a segunda escrita ser recusada em vez de virar um merge ruim.

## Comunidade

- Achou um caso extremo de coordenação? [Abra uma issue](https://github.com/tacyan/zaivern-code/issues).
- Usa um agente de código ainda sem suporte? [Peça uma integração](https://github.com/tacyan/zaivern-code/issues).
- Roda 8, 16, 32 ou 64 agentes? Compartilhe seu benchmark — `tools/conflict-bench.sh` e
  `tools/anyrepo-prove.sh` produzem números comparáveis aos das tabelas acima.
- Construiu algo com o Zaivern Code? Mostre sua configuração.

Pull requests são bem-vindos contra a `main` — o [CONTRIBUTING.md](CONTRIBUTING.md)
cobre compilar a partir do código (Rust 1.88+), verificar uma mudança e rodar as
checagens de Linux e Windows localmente.

Se o Zaivern Code for útil para você, uma ⭐ **Star** ajuda outras pessoas a encontrá-lo.

## Licença

[Apache License 2.0](LICENSE)
