<div align="center">

<img src="assets/Zaivern.png" width="120" alt="Zaivern Code" />

# Zaivern Code

**Um único cockpit para o Claude Code, o Codex, o Gemini CLI e os demais CLIs de programação com IA que você já usa.**<br>
Inicie, acompanhe e conduza todos eles a partir de um único aplicativo nativo no macOS, Windows e Linux.

[English](README.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md) | [한국어](README.ko.md) | **Português (Brasil)** | [Español](README.es.md)

[![Release](https://img.shields.io/github/v/release/tacyan/zaivern-code)](https://github.com/tacyan/zaivern-code/releases/latest)
[![CI](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml/badge.svg)](https://github.com/tacyan/zaivern-code/actions/workflows/test.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)

[**Download**](https://github.com/tacyan/zaivern-code/releases/latest) ·
[**Início rápido**](#início-rápido) ·
[**Documentação**](#documentação) ·
[**Site**](https://zaivern.com/)

<a href="https://zaivern.com/">
  <img src="assets/zaivern-demo.gif" width="960" alt="Zaivern Code executando Claude Code, Codex, Gemini CLI e outros agentes de programação lado a lado" />
</a>

<!-- 出典: docs/conflict-zero.md §3.12 — zaivern-code / 書き手 16 / zai 0.14.0:
     素の git 26 ファイル・28 ハンク、zaivern あり 0/0・96/96 成立・拒否 0・30 件ずらし -->
**16 agentes escrevendo neste repositório em paralelo** — git puro: **26 arquivos em conflito / 28 hunks**.<br>
Com o lease ledger: **0 / 0**, e todas as **96 edições foram aplicadas** — nenhuma recusada, sendo 30 delas deslocadas para um intervalo de linhas livre.<br>
[Veja as medições →](docs/conflict-zero.md)

Se o Zaivern Code te parecer útil, uma ⭐ **Star** ajuda o desenvolvimento dele.

</div>

## Por que o Zaivern Code

Iniciar vários CLIs de programação com IA é fácil. Difícil é acompanhá-los. Cada agente
vive na sua própria aba de terminal, pede aprovação no seu próprio ritmo e edita arquivos
sem saber o que os outros estão fazendo.

<!-- 出典: docs/conflict-zero.md §3.3 — 書き手 64 / 重なり 0.5:
     ベースラインは 57/64 のマージが衝突し 132 ハンク、ガード側は全規模で 0 ハンク -->

| Sem um cockpit | Com o Zaivern Code |
|---|---|
| Mais agentes em paralelo, mais conflitos de merge | Um ledger compartilhado mantém os agentes fora das linhas uns dos outros — 0 hunks em conflito com 64 agentes, onde o git puro produziu 132 |
| Percorrer as abas para descobrir quem precisa de você | Todos os agentes em uma tela, com status ao vivo |
| Colar a mesma instrução em cada ferramenta | Faça broadcast uma vez para toda a frota, ou direcione a um único agente |
| Perder um pedido de aprovação e perder a execução | Notificações e aprovação com um clique |
| Ficar preso à mesa enquanto os agentes trabalham | Acompanhe o progresso e aprove pelo celular |

O Zaivern Code não é um modelo de IA e não inclui nenhum. Ele conduz os CLIs que você já
instalou e nos quais já fez login — basta um para começar.

## Início rápido

**Pré-requisitos.** Instale e faça login em pelo menos um CLI de programação com IA
compatível. O Zaivern Code traz presets de inicialização para 33 deles, incluindo Claude
Code, Codex e Gemini CLI. Você não precisa de mais de um.

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

Ambos os instaladores verificam o arquivo da release contra o `checksums.txt` publicado
**antes de descompactá-lo**, e abortam sem extrair nem executar nada se o SHA-256 não
bater — ou se os checksums não puderem ser obtidos.

Prefere não canalizar um script para o seu shell? Baixe o arquivo da sua plataforma em
[Releases](https://github.com/tacyan/zaivern-code/releases/latest), descompacte-o e
coloque o `zai` (ou `zai.exe`) em algum lugar do seu `PATH`. Depois execute `zai .` na
pasta de um projeto. Veja [SECURITY.md](SECURITY.md) para saber como verificar o download
manualmente, conferir a proveniência do build ou ler o SBOM.

Com a janela aberta:

1. Clique em `+ Agent` e escolha um CLI que você já tenha instalado.
2. Digite uma tarefa na caixa de entrada e envie.
3. Adicione um segundo agente quando o primeiro já estiver confortável.

### Atualização

```bash
zai update            # verifica se há uma release mais nova, mostra o comando e então atualiza
zai update --check    # apenas verifica; não muda nada
zai update --yes      # atualiza sem o pedido de confirmação
```

O `zai update` funciona com o editor aberto ou fechado, e atualiza no lugar através do
script instalador da sua plataforma. Reexecutar o one-liner acima faz a mesma coisa.

O `zai uninstall` remove tudo (`--dry-run` lista o que seria removido). A desinstalação
toca apenas no executável e em `~/.zaivern`; qualquer outra coisa no seu `PATH` é apenas
listada, nunca apagada.

## Principais recursos

### Coordenação de conflitos (a razão de isto existir)

Os agentes reservam os arquivos — ou os intervalos de linhas individuais — que estão
prestes a editar em um ledger compartilhado por repositório, e os hooks do git recusam
uma escrita que colidiria.

<!-- 出典: docs/conflict-zero.md §3.8.1 — --layout disjoint / 64 体:
     B (ファイル単位の所有) 完了 1・拒否 63、Cref (行域) 完了 64・拒否 0・ハンク 0 -->
Os intervalos de linhas são o que torna isso viável em escala. Aponte 64 agentes para um
único arquivo e um lease em nível de arquivo deixa passar exatamente **1** deles,
recusando os outros **63**; com posse por região de linhas, todos os **64** passam, nada é
recusado, e o merge ainda produz **0** hunks em conflito.

<!-- 出典: docs/conflict-zero.md §3.12.2 — 錨の誤マッチによる二重配布と、その修正 -->
Uma região é rastreada por uma âncora — o conteúdo da sua primeira e da sua última linha —
e não por um número de linha, de modo que ela sobrevive a edições feitas acima dela. Se a
reresolução dessa âncora cair em um lugar diferente do que o ledger registrou, a leitura é
descartada em vez de considerada confiável, para que uma reserva nunca migre silenciosamente
para outra parte do arquivo.

Nada disso pega um conflito semântico; a [seção abaixo](#coordenação-de-conflitos) detalha
o que está coberto e o que não está.

### Agent Cockpit

Disponha vários CLIs de IA lado a lado e veja num relance qual deles está pensando,
editando, executando ou esperando por você. Presets de inicialização para 33 ferramentas
já vêm embutidos, então adicionar um agente é uma operação de dois cliques, e não uma
linha de comando que você precisa lembrar.

### Broadcast

Envie uma instrução para todos os agentes em execução a partir de uma única caixa de
entrada, ou escolha um agente quando quiser controle focado. Útil quando a mesma correção
se aplica à frota inteira.

### Status, aprovações e notificações

O Zaivern Code expõe pedidos de permissão, travamentos e saídas inesperadas como
notificações sobre as quais você pode agir com um clique. A aprovação automática vem
desligada por padrão e precisa ser ativada deliberadamente.

### Controle pelo celular

Acompanhe o progresso, envie instruções, aprove ações e edite arquivos pelo seu celular.
A configuração mais simples funciona na mesma rede Wi-Fi. Quando você não está nela, um de
dois transportes assume: **[Tailscale](https://tailscale.com/)**, se as duas máquinas já
estiverem na mesma tailnet, ou um túnel SSH através de um host que você já consegue
alcançar. Trocar de transporte muda apenas onde o servidor escuta — o token, a porta e a
página continuam os mesmos, então um QR já escaneado no celular continua funcionando.

O modo Tailscale não precisa de bastion nem de encaminhamento de portas: instale o
[Tailscale](https://tailscale.com/download) no PC e no celular, faça login em ambos na
mesma tailnet e clique em **🔒 Listen on Tailscale** na janela de controle pelo celular.
Ele faz bind no endereço da tailnet e em `127.0.0.1`, e em mais nada, de modo que o Wi-Fi
do café ou do aeroporto em que você estiver não consegue enxergar a porta. O Zaivern
descobre o endereço da tailnet pela tabela de rotas do kernel e nunca chama o comando
`tailscale` — no macOS esse CLI é um wrapper de shell que pode travar para sempre quando o
daemon não está acessível, e um processo filho travado congelaria a UI.

### Editor embutido

Leia código e revise o que seus agentes mudaram sem sair do aplicativo, incluindo imagens,
PDFs, CSVs e Markdown. Buffers não salvos sobrevivem a um crash: a próxima inicialização os
restaura e, se o arquivo tiver mudado em disco nesse meio-tempo, a diferença é mostrada a
você em vez de o conteúdo ser sobrescrito silenciosamente.

## Coordenação de conflitos

Uma reserva no ledger não é um conselho: o hook recusa a escrita conflitante no momento em
que ela é tentada, então o choque aparece ali, e não na hora do merge.

<!-- 出典: docs/conflict-zero.md §3.16.6 — dup_lines=0 は常に成立 (内容に依存しない)、
     conflict_files=0 は条件付き (帯 + 壁 + 昇順。反復的な内容では断ることがある) -->
Duas garantias valem em graus diferentes, e misturá-las exageraria a afirmação. "Dois
agentes nunca recebem as mesmas linhas" é uma propriedade do ledger e vale independentemente
do que os arquivos contêm. "O merge então passa em uma única vez" é condicional: exige uma
faixa de segurança, uma linha única entre as duas regiões e ordem crescente. Conteúdo
repetitivo pode quebrar a segunda enquanto a primeira continua valendo, e nesse caso o gate
recusa em vez de adivinhar.

O que ele não consegue pegar é um conflito semântico: um agente muda a assinatura de uma
função enquanto outro segue chamando a antiga, em um arquivo diferente, com um merge
perfeitamente limpo.

```console
$ zai czero init      # instala o ledger, os git hooks e o merge driver, e então faz o autodiagnóstico
$ zai czero verify    # cria um conflito real em um repositório descartável e confere se ele é barrado
```

Escopo, limites e as medições por trás deles estão em
[docs/conflict-zero.md](docs/conflict-zero.md).

## Uso de recursos

<!-- 出典: docs/idle-cost.md §7 — 2026-08-15、同一マシン・同一セッションで
     Zed 1.15.0 / zai 0.16.0 / zai 0.17.0 を交互に 3 ラウンド、9/9 VALID。
     0.16.0 を陽性対照に入れてあるので「測定が生きていること」まで示せる。
     0.17.0 は測定床に張り付いているので必ず「≤」で書くこと -->

Um editor que você deixa aberto o dia inteiro não deveria custar nada enquanto você não
está digitando. Medido em uma máquina, em uma única sessão, alternando entre os aplicativos
três vezes (macOS 26.5.2, na tomada, janelas de observação de 180 segundos, um workspace
neutro de 4 arquivos):

| | Zed 1.15.0 | Zaivern Code 0.17.0 |
|---|---:|---:|
| CPU ociosa (mediana de 3) | 0,761% de um núcleo | **≤0,006%** — no piso da medição |
| Download | 424,6 MB (`.app`) | **28,7 MB** (um binário) |
| RSS | 162,2 MB | 170,3 MB |

Duas coisas com as quais esta tabela toma cuidado:

- **`≤0,006%` é um piso, não uma leitura.** O `ps` resolve o tempo de CPU em 1/100 s, então
  uma janela de 180 segundos não consegue distinguir nada abaixo de 0,006%. As três rodadas
  caíram em exatamente um tick. A afirmação honesta é "pelo menos 127x menor que o Zed", e
  não uma razão exata.
- **RSS não é uma vitória, e não afirmamos que seja.** O Zed também é escrito em Rust; os
  dois ficam a menos de 5% um do outro, o que é ruído. O número que difere por uma ordem de
  grandeza é o tamanho do download.

A mesma execução mediu o Zaivern Code **0.16.0** em 8,933% — esse é o controle positivo.
Sem uma versão que produza uma leitura alta na mesma sessão, um resultado próximo de zero
não pode ser distinguido de uma medição quebrada. O custo em ociosidade caiu na 0.17.0
porque o tour guiado não reserva mais frames incondicionalmente, e o repaint de manutenção
a cada dois segundos foi eliminado.

Reproduza com `tools/idle-duel.sh --vs Zed --out /tmp/duel.tsv`. O harness se recusa a
medir quando não pode medir honestamente: ele verifica por pid se o aplicativo está em
primeiro plano, exige que a máquina esteja sem interação e registra a evidência em cada
linha. Método completo, números brutos e as armadilhas em que caímos estão em
[docs/idle-cost.md](docs/idle-cost.md).

## Plataformas suportadas

| Item | Suporte |
|---|---|
| SO | macOS arm64/x86_64, Linux x86_64/arm64, Windows x86_64 |
| CLIs de IA | 33 presets de inicialização, incluindo Claude Code, Codex e Gemini CLI |
| Rust | 1.88+ — apenas ao compilar a partir do código-fonte |
| Licença | Apache-2.0 |

Uma configuração comum é o Claude Code implementando, o Codex testando e o Gemini CLI
escrevendo a documentação, mas nada no Zaivern Code pressupõe essa divisão. Qualquer
combinação funciona, inclusive um único agente.

## Segurança

- O modo com aprovação obrigatória é o padrão; o Auto-YES é opt-in por sessão.
- Escalonamento de privilégios sempre exige aprovação manual.
- Os valores das variáveis de ambiente de MCP nunca são exibidos — apenas se estão definidos.
- Processos filhos são encerrados quando uma sessão é destruída ou o aplicativo sai, então
  nenhum agente órfão continua rodando em segundo plano.

## FAQ

**Qual a diferença em relação ao tmux com painéis divididos?**

O tmux organiza terminais em blocos; ele não faz ideia do que está rodando dentro deles. O
Zaivern Code lê o estado de cada agente, então consegue mostrar qual deles está pensando,
editando ou bloqueado em um pedido de aprovação, e transformar esse pedido em uma
notificação que você responde com um clique. A parte para a qual o tmux não tem equivalente
é o ledger compartilhado: dois agentes não conseguem fisicamente escrever nas mesmas linhas,
porque um git hook recusa a segunda escrita no momento em que ela é tentada, em vez de
deixá-la para ser descoberta na hora do merge.

**O lease ledger deixa as coisas mais lentas?**

<!-- 出典: docs/conflict-zero.md §1「意味しないこと」4 / §3.3 (掃引: 4〜8 体 p50 40〜50ms、
     64 体 p50 160ms、busy-deny 32 体 4 件・64 体 14 件) / §3.4 (ゲート 1536 回で p50 298.7ms)。
     体数だけでは決まらないので、必ず担当表の大きさを添えること -->
Sim, e piora com a escala, porque o gate fica no caminho da escrita. Na varredura padrão —
N escritores sobre N×6 arquivos — a latência do gate é p50 de 40–50 ms com 4–8 agentes e p50
de 160 ms com 64. A quantidade de agentes não é a única variável: uma tabela de atribuição
mais pesada, que chama o gate 1536 vezes, chega a p50 de 298,7 ms com esses mesmos 64
agentes, então qualquer número isolado do tipo "com 64 agentes custa X" fica incompleto sem
o tamanho da carga de trabalho. A partir de 32 agentes o gate também começa a responder
`busy-deny` quando não consegue decidir a tempo: ele recusa em vez de adivinhar, e uma nova
tentativa passa, mas você vê isso como uma rejeição ocasional. Com um ou dois agentes o gate
não está no seu caminho crítico.

**O que "zero conflitos" significa de fato?**

Algo mais estreito do que parece, deliberadamente:

<!-- 出典: docs/conflict-zero.md §3.2 (書き手 8 / 重なり 1.00: 10/48 成立・38 件をゲートが停止)、
     §3.8.1 (disjoint / 64 体: 素の git のハンクは全規模 0。B は完了 1、Cref は 64)、§3.16.6 -->
- **O zero é comprado recusando escritas.** Com oito escritores mirando todos nos mesmos
  arquivos, 10 de 48 edições planejadas foram escritas e as outras 38 foram barradas no
  gate. A contagem de conflitos é 0; a vazão não é.
- **Intervalos de linhas suficientemente distantes nunca precisaram de ajuda.** O git puro
  já faz o merge deles com zero conflitos. A posse por região de linhas não está fazendo algo
  que o git não consegue — ela devolve o paralelismo que um lease em nível de arquivo destrói
  (1 de 64 agentes passando, contra 64 de 64).
- **As duas garantias não têm a mesma força.** "Dois agentes nunca recebem as mesmas linhas"
  vale sempre; "o merge passa em uma única vez" é condicional e pode falhar com conteúdo
  repetitivo.

O [docs/conflict-zero.md](docs/conflict-zero.md) começa exatamente por essa fronteira e
carrega todas as medições por trás dela, incluindo as afirmações que foram posteriormente
refutadas.

## Documentação

| Documento | O que ele cobre |
|---|---|
| [docs/conflict-zero.md](docs/conflict-zero.md) | O que "livre de conflitos" afirma, o que não afirma, e as medições por trás disso |
| [docs/czero-repo-shapes.md](docs/czero-repo-shapes.md) | Quais garantias valem para qual formato de repositório |
| [docs/plugins.md](docs/plugins.md) | Como escrever plugins, com a [especificação do formato](docs/PLUGIN_SPEC.md) |
| [docs/README.md](docs/README.md) | Índice de todos os outros documentos, agrupados pela afirmação que sustentam |

As notas de lançamento de cada versão estão na
[página de Releases](https://github.com/tacyan/zaivern-code/releases).

## Contribuindo

Relatos de bugs, pedidos de recursos e pull requests são bem-vindos. Verifique as
[Issues](https://github.com/tacyan/zaivern-code/issues) em busca de um relato existente
antes de abrir um novo, e abra um
[Pull Request](https://github.com/tacyan/zaivern-code/pulls) contra a `main`.

```bash
git clone https://github.com/tacyan/zaivern-code.git
cd zaivern-code
rustup update stable
cargo run --release -- .
```

O [CONTRIBUTING.md](CONTRIBUTING.md) cobre o resto: como verificar uma alteração, como
rodar as checagens de Linux e Windows localmente, e as convenções que este repositório
segue.

## Licença

[Apache License 2.0](LICENSE)

---

<div align="center">

**Os agentes já são rápidos. Agora é a sua vez de comandar mais rápido.**

</div>
