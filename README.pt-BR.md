<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

### Rode vários agentes de código sem o caos dos conflitos de merge.

**Comece com 2 agentes. Escale para 64.**
O Zaivern Code barra edições sobrepostas antes que elas cheguem ao disco, então elas
nunca viram conflitos de merge.

Uma janela para o Claude Code, o Codex, o Gemini CLI e outras 30 CLIs de agente que
você já tem instaladas. Binário nativo único — macOS, Linux, Windows.

[English](README.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md) | [한국어](README.ko.md) | **Português (Brasil)** | [Español](README.es.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

</div>

**Instale e inicie**

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

Requer pelo menos uma CLI de código compatível já instalada e autenticada.
O Zaivern Code apenas conduz as CLIs que você já tem e não inclui modelo de IA nem assinatura.

**Coordenação de conflitos (opcional):**

```bash
zai czero init
```

Isso modifica o repositório Git atual.
[Pré-visualize e verifique as mudanças →](#ativar-a-coordenação-de-conflitos) ·
[Download manual e verificação](SECURITY.md)

<div align="center">

<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="O cockpit do Zaivern Code: várias CLIs de agente de código lado a lado em uma única janela, com o estado de cada agente" />
</a>

[**Início rápido**](#início-rápido) ·
[**Benchmarks**](#benchmarks-e-limitações) ·
[**Documentação**](#documentação) ·
[**Download**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**Site**](https://zaivern.com/)

</div>

*O vídeo acima é o cockpit — várias CLIs de agente em uma janela. Ele não mostra a
coordenação de conflitos; isso é medido separadamente, logo abaixo.*

## Prova

**64 agentes, um repositório, uma mesma carga de trabalho.** Arquivos = escritores × 6,
metade deles visada por mais de um agente. A mesma lista de tarefas executada duas vezes:
uma pelo git puro, outra pelo livro-razão de intervalos de linha do Zaivern Code.

| | Git puro | Zaivern Code |
|---|---:|---:|
| Merges que conflitaram | 57 de 64 | **0 de 64** |
| Hunks de conflito deixados para uma pessoa | 132 | **0** |
| Edições que entraram | 384 de 384 | 202 de 384 |
| Escritas barradas antes de entrar | 0 | 182 |

**O zero é comprado recusando escritas, não fundindo os dois lados por mágica.**
182 das 384 edições planejadas foram barradas no portão porque outro agente ativo já
era dono daquelas linhas; 14 das 182 foram recuos por contenção, que podem passar em uma nova tentativa.

**Quando os intervalos são de fato disjuntos, nada é recusado.** 64 agentes editando
64 intervalos separados de um *mesmo* arquivo entregam **64 de 64** edições, com **0**
recusas e **0** hunks de conflito — enquanto um lock por arquivo entrega 1 e recusa 63.

Conflitos semânticos **não** são detectados: um agente que muda uma assinatura enquanto
outro segue chamando a forma antiga passa, e o git faz o merge sem reclamar.

[Metodologia, números por escala, latência do portão e todas as lacunas em aberto →](docs/conflict-zero.md)

## O problema

Rodar um agente de código é fácil. Rodar quatro não é. **Dois agentes editando o mesmo
arquivo já bastam:**

- Eles editam as mesmas linhas, e você só descobre na hora do merge.
- Você não enxerga qual agente está trabalhando, bloqueado ou parado em silêncio.
- Um pedido de aprovação passa correndo numa aba que você não estava olhando.
- A integração vira trabalho seu — toda vez.

O gargalo não são os agentes. É a **coordenação entre eles**.

## A solução

O Zaivern Code coordena quais partes do repositório cada agente pode editar com segurança.
Em vez de descobrir colisões na hora do merge, ele pega o trabalho sobreposto **antes de a
escrita conflitante entrar** — e reúne num só lugar como observar, guiar e recuperar os
agentes que você tem rodando.

```text
Sem o Zaivern                            Com o Zaivern

Agent 1  ─┐                              Agent 1  ─┐
Agent 2  ─┤                              Agent 2  ─┤   ┌─────────────┐
Agent 3  ─┼─→ mesmos arquivos ─→ merge   Agent 3  ─┼─→ │ livro-razão │ ─→ integração
   ...   ─┤              com conflitos      ...   ─┤   │  de linhas  │    limpa
Agent 64 ─┘                              Agent 64 ─┘   └─────────────┘
```

## Início rápido

### Iniciar o cockpit multiagente

Instale com o comando de uma linha no topo desta página e rode `zai .` na pasta de um
projeto. O cockpit abre naquela pasta — painéis de agente, editor, controle pelo celular.
Clique em `+ Agent`, escolha uma CLI que você tenha instalada e mande uma tarefa.
**Isso não liga a coordenação de conflitos**; esse é o passo seguinte.

Os instaladores conferem o arquivo baixado contra o `checksums.txt` da release
**antes de descompactar**, e abortam se não bater.
[Download manual, verificação de checksum, proveniência e SBOM →](SECURITY.md)

### Ativar a coordenação de conflitos

```bash
zai czero init --dry-run  # pré-visualiza as mudanças planejadas
zai czero init            # instala o livro-razão e a integração com o Git
zai czero verify          # verifica em repositórios descartáveis
zai .                     # inicia o cockpit
```

- **`zai czero init --dry-run`** pré-visualiza as mudanças planejadas sem modificar
  o repositório atual.
- **`zai czero init` modifica o repositório Git atual.** Ele prepara o livro-razão de
  intervalos de linha, adiciona os hooks `pre-commit` / `pre-applypatch` /
  `pre-merge-commit`, registra o union merge driver e escreve um bloco gerenciado no
  `.gitattributes` — e então se autodiagnostica. É idempotente.
- **`zai czero verify`** cria escritas sobrepostas reais e merges reais em repositórios
  descartáveis e confere se cada um é de fato barrado. **Ele não modifica o repositório
  atual.** O veredito é `verified` / `partial` / `broken` — ele não reporta "verified"
  para um teste que não conseguiu executar.
- **`zai czero doctor`** diagnostica quais camadas seguem ativas, e
  **`zai czero uninstall`** remove exatamente o que o `init` adicionou.

### Atualização

`zai update` mostra o comando que vai executar e então atualiza (`--check` só consulta,
`--yes` pula a confirmação). Funciona com o editor aberto ou fechado.
`zai uninstall` remove tudo.

## Principais recursos

Ordenados por quanto diferenciam o Zaivern Code. O primeiro é a razão de ele existir.

### 1. Posse de arquivos e intervalos de linha, imposta na hora da escrita

Os agentes reivindicam arquivos ou intervalos de linha antes de editar, ancorados ao
**conteúdo ao redor** e não ao número da linha. Se outro agente ativo já é dono de uma
região sobreposta, um hook do git recusa a escrita — na hora da escrita, não na do merge.
Mesmo arquivo, linhas diferentes é permitido, e é isso que mantém os agentes em paralelo
em vez de serializá-los atrás de um lock do arquivo inteiro.
[Como funciona a coordenação por intervalo de linha →](docs/conflict-zero.md)

### 2. Uma tela, e dá para ver o que cada agente está fazendo

Coloque várias CLIs de IA lado a lado e veja num relance qual está pensando, editando,
executando ou esperando por você. Adicionar um agente são dois cliques, não uma linha de
comando decorada.

### 3. Detecção de travamento e de saída

O Zaivern Code observa progresso semântico, não pixels: um agente que para de avançar é
reportado como **travado**, e saídas inesperadas aparecem como notificações.

### 4. Instrução em massa e direcionada

Mande uma instrução para todos os agentes em execução a partir de um único campo, ou mire
em um agente quando quiser controle focado.

### 5. Aprovações

O modo com aprovação obrigatória é o padrão. O auto-YES é opt-in por sessão, a elevação de
privilégio sempre precisa de uma pessoa, e valores de variáveis de ambiente de MCP nunca
são exibidos.

### 6. Controle pelo celular

Acompanhe o progresso, envie instruções, aprove ações e edite arquivos pelo celular.
Use o mesmo Wi-Fi, o [Tailscale](https://tailscale.com/) ou um túnel SSH.

### 7. Editor embutido

Revise código e mudanças dos agentes sem sair do Zaivern Code, incluindo Markdown,
imagens, PDFs e CSVs. Buffers não salvos são recuperados depois de um crash.

Também inclui: plugins e uma interface em seis idiomas.
[Docs de plugins](docs/plugins.md) · [Docs de tradução](docs/translating.md)

## Como funciona

1. **Inicie** agentes de código a partir de uma janela, ou anexe os que você já roda.
2. **Reivindique** arquivos ou intervalos de linha antes de editar, ancorados ao conteúdo ao redor.
3. **Barreira** — um hook do git recusa uma escrita sobreposta antes que ela chegue ao merge.
4. **Integre** — mudanças que não se sobrepõem entram pelo git como de costume.

## Agentes compatíveis

Claude Code · Codex · Gemini CLI · Cursor Agent · GitHub Copilot CLI ·
**mais 28** — 33 presets de inicialização no total, além de 6 agentes conduzíveis por ACP.

Qualquer combinação funciona, inclusive um único agente.
Falta o seu? [Peça uma integração](https://github.com/tacyan/zaivern-code/issues).

## Por que o Zaivern

|  | Multiplexador de terminal | Painel genérico de agentes | Zaivern Code |
|---|:---:|:---:|:---:|
| Posse de intervalo de linha + recusa na escrita | ❌ | ❌ | ✅ |
| Sabe o estado do agente (pensando / bloqueado / travado) | ❌ | varia | ✅ |
| Uma tela para todos os agentes de uma vez | ❌ | ✅ | ✅ |
| Aprovações como notificações | ❌ | varia | ✅ |
| Controle por celular / remoto | ❌ | varia | ✅ |
| Binário nativo único, sem runtime | varia | varia | ✅ |

## Benchmarks e limitações

A tabela de 64 agentes lá em cima é sintética. Em **repositórios reais**, clonados e
reexecutados pelo `tools/anyrepo-prove.sh` com 16 escritores (zai 0.14.0):

| Repositório | Git puro | Zaivern Code |
|---|---|---|
| zaivern-code (Rust, 259 arquivos rastreados) | 26 arquivos em conflito / 28 hunks | **0 / 0** — 96 de 96 edições entraram, 0 recusas, 30 deslocadas |
| hyperframes (TS/HTML, 1.194 arquivos rastreados) | 26 / 28 | **0 / 0** — 96 de 96 entraram, 0 recusas, 32 deslocadas |

Recusar não é o único desfecho. Quando uma reivindicação colide, o `--shift` a move para o
intervalo livre mais próximo com a mesma largura — é por isso que as duas linhas acima
entregam todas as edições e não recusam nenhuma.

### O que "zero conflitos" quer dizer

- **A posse sempre vale.** "Duas pessoas nunca recebem as mesmas linhas" depende só do
  livro-razão, não do conteúdo do arquivo: `dup_lines = 0` em 126 de 126 execuções
  independentes da prova.
- **Um merge limpo é condicional.** Em conteúdo repetitivo — cercas de código repetidas,
  código gerado, a mesma linha várias vezes — o git ainda pode conflitar mesmo com os
  intervalos suficientemente distantes. O portão **recusa essas reivindicações** em vez de
  prometer um merge que não pode garantir.
- **Conflitos semânticos estão fora do escopo.** O que se impede é a sobreposição de posse
  de linhas; uma assinatura alterada e um chamador desatualizado em outro arquivo, não.
- **Trabalho disjunto nunca precisou de ajuda.** Intervalos suficientemente distantes já
  entram com zero conflitos no git puro. A posse por intervalo de linha devolve o
  **paralelismo que um lock por arquivo destrói** — essa é a comparação que importa.
- **Só há imposição onde o git consegue impor.** O `zai lease claim` também tem sucesso em
  uma pasta que não é git, mas ali nada é barrado. O `zai czero doctor` informa quais
  formatos de repositório (worktrees, submodules, sparse-checkout, LFS, bare) estão de fato cobertos.

Reproduza qualquer um deles: `tools/conflict-bench.sh`, `tools/coedit-bench.sh`,
`tools/anyrepo-prove.sh --repo .`
[Metodologia completa e lacunas restantes →](docs/conflict-zero.md) ·
[quais garantias valem para qual formato de repositório →](docs/czero-repo-shapes.md)

## Plataformas compatíveis

| Item | Suporte |
|---|---|
| SO | macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64 |
| Distribuição | Binário nativo único, sem runtime; checksums, SBOM e proveniência de build a cada release |
| CLIs de IA | 33 presets de inicialização, mais 6 por ACP |
| Testes | 5.005 na v0.23.0, executados em macOS, Linux e Windows no CI |
| Licença | Apache-2.0 |

## Documentação

| Documento | O que cobre |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | O que "livre de conflitos" afirma, o que não afirma, e cada medição por trás disso |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | Quais garantias valem para qual formato de repositório |
| [docs/idle-cost.md](docs/idle-cost.md) | Como CPU em repouso e tamanho do binário são medidos |
| [docs/plugins.md](docs/plugins.md) | Como escrever plugins, com a [especificação do formato](docs/PLUGIN_SPEC.md) |
| [docs/README.md](docs/README.md) | Índice de todos os outros documentos, agrupados pela afirmação que sustentam |

[Notas de release](https://github.com/tacyan/zaivern-code/releases) ·
[Política de segurança](SECURITY.md) · [Como contribuir](CONTRIBUTING.md)

## Experimente

Experimente o Zaivern Code com dois agentes no mesmo repositório:

```bash
zai czero init
zai .
```

Inicie dois agentes, aponte os dois para o mesmo arquivo e veja a segunda escrita
sobreposta ser recusada *antes* de virar um conflito de merge. É essa a ideia inteira,
em cerca de um minuto.

Se funcionar para você, uma ⭐ **Star** ajuda outras pessoas a encontrar o projeto.

## Comunidade

- Achou um caso-limite de coordenação? [Abra uma issue](https://github.com/tacyan/zaivern-code/issues).
- Usa um agente de código ainda não suportado? [Peça uma integração](https://github.com/tacyan/zaivern-code/issues).
- Rodando 8, 16, 32 ou 64 agentes? Compartilhe seus números — o `tools/conflict-bench.sh`
  e o `tools/anyrepo-prove.sh` produzem resultados comparáveis às tabelas acima.

Pull requests são bem-vindos na `main` — o [CONTRIBUTING.md](CONTRIBUTING.md) cobre como
compilar a partir do código (Rust 1.88+), como verificar uma mudança e como rodar as
checagens de Linux e Windows localmente.

## Licença

[Apache License 2.0](LICENSE)
