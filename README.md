# ARGOS

Monitor de hardware em desenvolvimento para Windows 11, escrito em Rust. O ARGOS oferece uma janela com gráficos e modos de diagnóstico e coleta contínua no console. A GPU de referência é a NVIDIA GeForce RTX 5060 Ti de 16 GB.

As etapas do projeto e os critérios para avançar estão no [roadmap](ROADMAP.md).

## Distribuição para Windows

Abra `ARGOS-Setup.exe` no Windows 11 x64 e escolha uma opção.

- **Somente executar** abre o painel a partir de uma pasta temporária. Se o painel estiver fechado e houver uma instalação, você pode escolher entre a versão instalada e a versão do pacote, inclusive quando o pacote for mais antigo. Os arquivos extraídos são removidos ao fechar o painel.
- **Instalar** adiciona o ARGOS à conta atual e cria um atalho no menu Iniciar. Em CPUs compatíveis, o instalador prepara a leitura de temperatura e potência com uma confirmação de administrador do Windows. Para desinstalar, abra **Configurações → Aplicativos → Aplicativos instalados**.

Na versão instalada, selecione **Iniciar com o Windows** no menu do painel para abrir o ARGOS ao entrar na sua conta. Selecione a opção novamente para desativar.

O painel mantém uma janela por conta do Windows. Se já houver uma janela aberta, pedir para executar uma versão igual ou mais antiga mostra o painel atual. Uma versão mais nova oferece a opção de trocar após fechar o painel. Ao instalar com o painel aberto, o instalador pede para fechá-lo e bloqueia a substituição de uma instalação por uma versão mais antiga. As duas opções usam as preferências do mesmo perfil do Windows.

![Tela inicial do instalador do ARGOS com as opções Somente executar e Instalar](docs/screenshots/instalador.jpg)

## Compilar e executar

Use o Rust estável com o alvo `x86_64-pc-windows-msvc` e as ferramentas de compilação C++ da Microsoft. O executável incorpora o runtime C e carrega a NVML ou a ADLX fornecida pelo driver para ler as GPUs NVIDIA e Radeon. A janela usa Win32 e GDI por meio das declarações da biblioteca `windows-sys`. Em execução, ela usa as bibliotecas do Windows. [Rust for Windows](https://github.com/microsoft/windows-rs)

```powershell
cargo build --release --locked
.\target\release\argos.exe gui
.\target\release\argos.exe
.\target\release\argos.exe --gpu 0 --samples 5 --interval-ms 1000
```

O comando padrão consulta todas as GPUs NVIDIA e Radeon enumeradas pelos drivers. Cada GPU recebe uma leitura dos sensores básicos. Para NVIDIA, o ARGOS também tenta ler os contadores GPM, com 1 s entre as coletas. `--samples` aumenta o número de intervalos GPM por GPU. `--interval-ms` aceita a partir de 101 ms, pois a NVIDIA exige mais de 100 ms entre amostras. [Funções GPM da NVML](https://docs.nvidia.com/deploy/nvml-api/latest/api/group__nvmlGpmFunctions.html)

### Gerar o instalador

Instale o Inno Setup 6.7 ou mais recente e execute o comando abaixo na raiz do projeto. Se `ISCC.exe` estiver no PATH, omita `-InnoCompiler`. [Inno Setup](https://jrsoftware.org/isdl.php)

```powershell
.\packaging\build.ps1 -InnoCompiler 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe'
```

O pacote gerado fica em `target\dist\ARGOS-Setup.exe`.

### Atualizações pelo painel

O ARGOS verifica novas versões estáveis em segundo plano ao abrir. No menu do painel, use **Buscar por atualizações** para consultar novamente. O resultado aparece no centro da janela e o painel continua recebendo cliques. Quando o pacote estiver pronto, escolha **Atualizar e reiniciar** ou **Atualizar ao fechar**. O painel mostra as novidades na primeira abertura depois da atualização. A instalação usa o mesmo pacote em modo silencioso. No modo **Somente executar**, a versão verificada fica no perfil do Windows e é reutilizada nas próximas aberturas pelo lançador. Se também houver uma cópia instalada, **Escolher outra versão** oferece a cópia instalada e a versão do pacote aberto.

## Janela de monitoramento

![Painel do ARGOS com gráficos de temperatura, potência, atividade da GPU, VRAM, CPU e RAM](docs/screenshots/painel.jpg)

Abra o painel pelo instalador, pelo menu Iniciar ou pelo comando `gui`. Na primeira abertura, o guia rápido mostra os principais controles e o painel usa um monitor secundário, quando disponível. Clique com o botão direito em qualquer área para abrir as opções de configuração e os comandos da janela.

O ícone do ARGOS fica na área de notificação, perto do relógio. Clique nele para mostrar o painel depois de minimizar. Pelo botão direito no ícone, escolha **Mostrar ARGOS** ou **Fechar ARGOS**. Enquanto está aberto no modo normal, o painel também aparece na barra de tarefas e pode ser encaixado pelo FancyZones.

Em **GPUs monitoradas**, marque as placas NVIDIA e Radeon que quer acompanhar ao mesmo tempo. O índice distingue placas do mesmo modelo. O modo automático coleta uma placa, com preferência pela NVIDIA. Cada placa tem seu próprio histórico e sua própria seção no menu **Sensores**. Ao desmarcar uma placa, seus gráficos ficam no layout e as demais placas continuam com seus históricos. CPU, RAM e quadros são coletados uma vez para todo o painel. Escolhas e gráficos são restaurados quando a placa tem uma identidade persistente. `gui --gpu 0` seleciona a primeira NVIDIA pelo índice NVML e `gui --gpu amd:0` seleciona a primeira Radeon pelo índice ADLX.

Em **Aparência**, escolha o tema claro ou escuro. Em **Aparência → Opacidade**, ajuste o fundo entre 0% e 100% com o controle deslizante. O painel normal e o dropdown usam a mesma configuração. Os textos, números e curvas continuam opacos, e as curvas mantêm as cores configuradas. Só o tema claro usa uma tonalidade cinza atrás das curvas. Abaixo de 100%, clique na faixa de 10 pixels no topo do painel aberto para acessar o menu. A barra de rolagem à direita também recebe cliques e arrastes quando há gráficos abaixo da área visível. Fora dessas áreas, os cliques passam para o aplicativo de baixo.

Os sensores Radeon usam a ADLX do driver AMD. Temperatura, uso, clocks, VRAM e potência total da placa aparecem conforme as capacidades do dispositivo. **RAM compartilhada da GPU** tem um gráfico separado da VRAM. A coleta respeita a cadência do driver AMD. Os contadores GPM e os motores NVENC/NVDEC pertencem às GPUs NVIDIA. [Métricas AMD ADLX](https://gpuopen.com/manuals/adlx/adlx-sdk-references/adlx-interfaces/performance-monitoring/iadlxgpumetrics/)

Os gráficos de codificação e decodificação de vídeo das Radeon usam os contadores que o Windows disponibiliza para a GPU selecionada. Se o driver reportar um motor compartilhado, o ARGOS mostra o uso desse motor. O menu **Sensores** lista os motores assim que o Windows os reporta, o que pode ocorrer só quando algum aplicativo usa vídeo por hardware naquela placa. As leituras são atualizadas no máximo uma vez por segundo. [Tipos de motor de GPU no Windows](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmdt/ne-d3dkmdt-dxgk_engine_type)

Para mudar a posição da janela, selecione **Mover janela**, clique no painel e arraste com o botão esquerdo pressionado. Use o gesto configurado no **FancyZones** durante o arraste para encaixar o painel em uma zona. Pressione `Esc` para cancelar. Arraste uma borda ou um canto para redimensionar. Ao reabrir, o painel restaura a última posição.

- **Intervalo de amostragem** escolhe 100, 200, 500 ou 1.000 ms. A troca preserva o histórico. Quando a opção é 100 ms, o coletor GPM respeita seu mínimo de 101 ms. [Funções GPM da NVML](https://docs.nvidia.com/deploy/nvml-api/latest/api/group__nvmlGpmFunctions.html)
- **Grade** (`F4`) ajusta de 1 a 6 colunas e de 1 a 4 linhas visíveis. Na primeira abertura, o ARGOS mostra quatro gráficos numa linha, na ordem de uso da CPU, temperatura da CPU, RAM usada e FPS. Novos sensores ocupam os espaços livres. Use a roda do mouse ou a barra de rolagem à direita para ver os gráficos que ficam abaixo.
- **Sensores** (`F2`) escolhe as curvas visíveis. Com a opacidade em 100%, clique com o botão direito num gráfico e use **Mesclar com** para sobrepor curvas da mesma unidade. O menu lista as curvas do painel e oferece **Separar gráficos** para devolver cada uma ao seu próprio painel.
- **FPS** segue o aplicativo em foco por padrão e mostra o estado da captura. Você pode escolher um executável específico ou usar **Desativado** para encerrar a captura. Na seção **Gráficos exibidos**, marque separadamente o FPS, o frametime e a espera da CPU. Em **Teto do gráfico**, ajuste o limite superior da escala de FPS. O valor padrão é 360 FPS.
- **Modo dropdown** recolhe o painel no topo do monitor escolhido. Ajuste a posição e a largura da faixa no menu e use **Mostrar faixa de ativação** para localizá-la. O painel abre ao manter o cursor na faixa e recolhe quando você se afasta. Um atalho global configurável alterna entre aberto e recolhido. A grade e os gráficos são os mesmos do modo normal, com coleta contínua nos dois modos.
- **Histórico** (`F3`) alterna entre 30 segundos, 1 minuto, 2 minutos e 5 minutos.
- Arraste um gráfico para posicioná-lo em um espaço livre ou para trocar de lugar com outro. O menu **Mover painel para** também permite escolher uma posição em outra página.
- Ao clicar em um gráfico, o menu também permite escolher ou restaurar a cor de cada curva, inclusive em gráficos mesclados. Com transparência, abra o menu pela faixa superior e escolha **Aparência → Cor dos gráficos** para trocar a cor de qualquer curva do painel.

O menu permanece aberto ao ajustar as configurações. Clique fora dele ou pressione `Esc` para fechá-lo.

O traçado ocupa a maior parte do painel, com fundo conforme o tema, quadriculado fino e unidade ao lado do valor atual. A escala vertical usa limites próprios de cada sensor. Percentuais e temperatura vão de 0 a 100. Memória usa a capacidade detectada, e potência e clocks usam limites informados pelo driver quando disponíveis. Uma leitura atual fora da escala mantém seu valor e recebe uma seta. [Consultas de dispositivos da NVML](https://docs.nvidia.com/deploy/nvml-api/latest/api/group__nvmlDeviceQueries.html)

Mínimo, máximo e média simples usam as leituras válidas do período selecionado. Leituras ausentes aparecem como lacunas, e a janela indica quando a coleta está atrasada. A coleta segue em segundo plano enquanto a janela está minimizada.

O ARGOS salva a posição e o tamanho da janela, a grade, as posições dos gráficos, os sensores, as mesclagens, as cores, a frequência e o período em `%LOCALAPPDATA%\ARGOS\dashboard-multi.cfg`. Na primeira abertura, ele reaproveita o layout de `dashboard.cfg`, usado pela 0.2.x.

### FPS

O ARGOS começa no modo **Aplicativo em foco**. Enquanto você interage com o painel, o ARGOS mantém o último aplicativo acompanhado. O menu **FPS** permite escolher outro modo de captura para a sessão atual. A escolha do aplicativo e a visibilidade dos gráficos são independentes. A captura usa eventos do Windows para chamadas `Present()` do DXGI e do Direct3D 9. O Windows exige execução como administrador ou uma conta no grupo **Performance Log Users** para essa sessão. O menu mostra quando a captura está indisponível. [Permissões do ETW](https://learn.microsoft.com/en-us/windows/win32/api/evntrace/nf-evntrace-starttracew#return-value)

| Sensor | Medição |
| --- | --- |
| FPS do aplicativo | Taxa calculada a partir dos intervalos entre apresentações bem-sucedidas na cadeia de apresentação mais ativa do processo. |
| Intervalo entre quadros (pico) | Maior intervalo entre inícios de chamadas `Present()` na janela de amostragem mais recente. |
| Tempo em Present() (pico) | Maior tempo que a thread passou dentro de `Present()` na janela de amostragem mais recente. |

O FPS usa uma janela de até 1 s. Os dois picos usam o **Intervalo de amostragem**. As medições se referem a apresentações do aplicativo. Quadros efetivamente exibidos, quadros gerados pelo driver e espera da GPU exigem outras fontes de eventos. O Windows pode entregar eventos em lotes, mesmo com o painel configurado para atualizar a cada 100 ms. Os tempos são calculados a partir dos relógios dos próprios eventos. [Entrega de buffers do ETW](https://learn.microsoft.com/en-us/windows/win32/api/evntrace/ns-evntrace-event_trace_properties) · [Cálculo da espera no PresentMon](https://github.com/GameTechDev/PresentMon/blob/main/IntelPresentMon/CommonUtilities/mc/MetricsCalculatorCpuGpu.cpp#L35-L49)

### Ping

No painel, abra **Sensores → Rede do jogo / Windows** e marque **Ping**. O sensor acompanha o aplicativo selecionado no menu **FPS**. Se você não fixar um aplicativo, o Ping segue o aplicativo em foco, inclusive com o FPS desativado. O ARGOS usa o tráfego nos dois sentidos para identificar o destino provável e mostra o IP ao lado do título do gráfico. A descoberta usa metadados do ETW do Windows e pode exigir execução como administrador. O menu **Sensores** mostra o destino completo, o método e o motivo de indisponibilidade. [Captura de rede no Windows](https://learn.microsoft.com/en-us/windows/win32/etw/udpip)

O gráfico amostra em milissegundos, uma vez por segundo. Para TCP, o Windows fornece o RTT suavizado da conexão. Para UDP, o ARGOS agrupa o tráfego entre os mesmos IPs, mesmo quando as portas mudam, e envia uma sondagem ICMP ao destino observado enquanto o sensor estiver ativo. O gráfico mantém o IP e mostra uma lacuna quando a sonda fica sem resposta. Também preserva as amostras anteriores dentro do período selecionado, com lacunas nas trocas de IP. [RTT TCP no Windows](https://learn.microsoft.com/en-us/windows/win32/api/tcpestats/ns-tcpestats-tcp_estats_path_rod_v0) · [Sondagem ICMP no Windows](https://learn.microsoft.com/en-us/windows/win32/api/icmpapi/)

O IP pode ser de um jogador, de um relay ou de um servidor. A leitura ICMP mede o trajeto até o endereço exibido. No Warframe, uma missão pode ser hospedada por um jogador. O Elite Dangerous calcula sua própria média de latência a servidores e jogadores. [Warframe](https://www.warframe.com/uk/patch-notes/pc/37-0-0) · [Frontier](https://customersupport.frontier.co.uk/hc/en-us/articles/4405945325970-Network-Options-Configuration)

### Ping personalizado

Em **Sensores → Ping personalizado → Editar endereços…**, informe até quatro destinos, um por linha. A lista aceita IPv4, IPv6 e nomes de host. O painel salva os endereços e mostra uma curva por destino no mesmo gráfico. As sondagens ICMP rodam em segundo plano uma vez por segundo enquanto o gráfico está ativo. Destinos sem resposta aparecem como lacunas.

### Download e upload

Escolha o adaptador em **Interfaces de rede**. O painel salva a escolha e oferece os gráficos **Download** e **Upload** em **Sensores**. As curvas mostram, em Mbps, o tráfego total recebido e enviado pela interface selecionada, incluindo transferências na rede local. Cada escala usa a velocidade do link que o Windows informa para a direção correspondente. Quando essa velocidade está indisponível, a escala acompanha o maior valor medido no histórico. A coleta registra uma amostra por segundo e começa um novo histórico quando muda de interface. [Contadores de interface do Windows](https://learn.microsoft.com/en-us/windows/win32/api/netioapi/ns-netioapi-mib_if_row2)

### Sensores de CPU

O painel consulta o sensor de controle (`Tctl`) de AMD Zen das famílias `17h`, `19h` e `1Ah` ou o sensor digital do pacote de CPUs Intel que oferecem essa leitura. A coleta acompanha o intervalo configurado no painel enquanto o gráfico está selecionado.

**Potência da CPU** mostra a potência média do pacote, em watts, calculada pela diferença entre duas leituras do contador de energia. O sensor está no menu **Sensores → CPU / PawnIO** e precisa do componente **ARGOS Sensor de CPU** atualizado quando o serviço já está instalado. A disponibilidade depende do processador. [Intel RAPL](https://www.intel.com/content/www/us/en/developer/articles/technical/software-security-guidance/advisory-guidance/running-average-power-limit-energy-reporting.html) · [Registradores AMD](https://www.amd.com/content/dam/amd/en/documents/processor-tech-docs/programmer-references/56255_OSRR.pdf)

O `ARGOS-Setup.exe` provisiona o driver assinado PawnIO e o serviço local **ARGOS Sensor de CPU** na primeira instalação ou atualização compatível. O painel usa as permissões normais da conta e o serviço inicia sob demanda. **Somente executar** aproveita um serviço já instalado. Se a leitura estiver indisponível, o gráfico mostra **N/D** e o menu **Sensores** informa o motivo. [Driver PawnIO](https://pawnio.eu/) · [Módulos oficiais](https://github.com/namazso/PawnIO.Modules/releases/tag/0.2.11)

Em **Aplicativos instalados**, **ARGOS Sensor de CPU** remove o serviço compartilhado pelas cópias do ARGOS nesta máquina. O PawnIO tem sua própria entrada, pois pode ser usado por outros programas.

## Coleta contínua

```powershell
.\target\release\argos.exe watch
.\target\release\argos.exe watch --gpu 0 --gpu-interval-ms 500 --gpm-interval-ms 1000 --system-interval-ms 2000 --duration-secs 20
```

`watch` lê os sensores básicos das GPUs escolhidas, os contadores GPM em NVIDIA e o uso de CPU e RAM. Repita o argumento para coletar mais de uma placa, como em `watch --gpu 0 --gpu amd:0`. Sem seleção explícita, o modo automático tenta NVIDIA primeiro e usa Radeon quando necessário. **RAM disponível** é a memória física que o Windows pode reutilizar imediatamente, incluindo o cache em espera. A leitura também está no painel, em **Sensores → Sistema / Windows**. Cada grupo usa inicialmente um intervalo de 1 s e tem seu próprio ajuste. [Memória física disponível no Windows](https://learn.microsoft.com/pt-br/windows/win32/api/sysinfoapi/ns-sysinfoapi-memorystatusex)

A primeira leitura de CPU e dos contadores GPM aguarda duas amostras. Falhas temporárias e métricas indisponíveis deixam lacunas no histórico. O horário exibido é o tempo decorrido desde o início da coleta. Para GPM, a saída também indica o intervalo observado entre amostras. O uso de CPU é calculado com os tempos ocupado e ocioso do Windows. Em máquinas com múltiplos grupos de processadores, essa leitura fica indisponível, pois a API consultada cobre apenas o grupo primário. [Tempos de CPU do Windows](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getsystemtimes)

O histórico retém até 5 minutos por grupo em memória. O console recebe as atualizações de forma independente da coleta, com uma fila limitada. Pressione `Ctrl+C` ou use `--duration-secs` para encerrar e ver o resumo. Use `watch --help` para consultar os intervalos aceitos.

Na sessão de validação de 30 min e 5 s, com intervalo de 1 s por grupo e saída redirecionada, cada grupo reteve 298 de 300 amostras possíveis na janela final. O processo usou em média 0,082% de um processador lógico e chegou a 31,35 MiB nas medições de memória residente. Esse teste serve de referência para o modo console. O impacto nos tempos de quadro será medido separadamente.

## Como interpretar as leituras

- O inventário usa as APIs do Windows para identificar o sistema, a CPU, os processadores lógicos ativos e a RAM utilizável.
- A NVML fornece o modelo da GPU, as versões do driver e da biblioteca, o modo do driver, a VRAM, a atividade da GPU e da VRAM, a temperatura, a potência, os clocks e a atividade de NVENC e NVDEC. Cada API tem sua própria janela de medição. Por exemplo, o driver retorna a potência média de 1 s na GPU de referência. [Consultas de dispositivos da NVML](https://docs.nvidia.com/deploy/nvml-api/latest/api/group__nvmlDeviceQueries.html)
- A consulta de suporte GPM é feita por GPU. Quando há suporte, duas amostras alimentam o cálculo dos cinco contadores percentuais abaixo. Uma leitura válida de `0%` é exibida como valor. A sonda de diagnóstico mostra o motivo disponível quando a consulta, a coleta ou um contador individual falha. [Definições dos contadores GPM](https://docs.nvidia.com/deploy/nvml-api/latest/api/group__nvmlGpmEnums.html)

| ID GPM | Métrica | O que o driver mede |
| --- | --- | --- |
| 2 | SMs ativos | Percentual de SMs ocupados |
| 3 | Ocupação de warps nos SMs | Warps ativos em relação ao máximo teórico |
| 5 | Atividade Tensor | Fração do tempo em que os SMs executaram operações Tensor |
| 10 | Uso da banda da VRAM | Fração da largura de banda teórica da VRAM utilizada |
| 12 | Atividade FP32 | Fração do tempo em que os SMs executaram operações FP32 fora das unidades Tensor |

## Validação inicial

Na RTX 5060 Ti de 16 GB, com Windows 11 build 26320 e driver NVIDIA 621.14 em modo WDDM, a NVML informou suporte a GPM e reportou 16.311 MiB de VRAM. O executável compilado em modo release, rodando sem elevação, obteve os cinco contadores tanto na janela padrão de 1 s quanto em cinco janelas consecutivas configuradas para 101 ms.

Na janela de 1 s, o driver reportou 11,81% de atividade FP32, 0,31% de atividade Tensor, 62,89% de atividade dos SMs, 29,25% de ocupação dos SMs e 22,65% da largura de banda teórica da DRAM. A execução usou a atividade normal da máquina, sem carga controlada. A precisão sob cargas conhecidas e o impacto nos frametimes ainda precisam ser medidos.

## Reproduzir o diagnóstico

1. Rode a sonda com `--samples 5 --interval-ms 1000` e registre o modelo da GPU, o driver, o modo e os estados dos contadores em repouso.
2. Repita durante uma carga conhecida. Para validar a resposta dos contadores, mantenha a carga estável por vários intervalos e use cargas que exercitem operações FP32 e Tensor em execuções separadas.
3. Se quiser comparar com a ferramenta da NVIDIA, faça uma execução separada com `nvidia-smi dmon -i 0 -c 5 -d 1 --gpm-metrics 2,3,5,10,12 --gpm-options d`, nos drivers que aceitam essa opção. O `nvidia-smi` é opcional para a comparação. [Referência do NVIDIA System Management Interface](https://docs.nvidia.com/deploy/nvidia-smi/index.html)

RT Cores, cache L2 e os tempos de GPU por quadro seguem em investigação. A captura de apresentações ainda precisa ser validada durante um jogo e medida quanto ao impacto nos tempos de quadro.

## Licença

O código do ARGOS é distribuído sob a licença MIT. Os módulos PawnIO seguem a licença LGPL 2.1 ou posterior. Os avisos e fontes correspondentes acompanham o pacote. [Licença do ARGOS](LICENSE) · [Avisos do PawnIO](packaging/pawnio/NOTICE.txt)
