# ARGOS

Monitor de hardware em desenvolvimento para Windows 11, escrito em Rust. O primeiro protótipo é uma sonda de console para descobrir quais sensores o driver disponibiliza. A GPU de referência é a NVIDIA GeForce RTX 5060 Ti de 16 GB.

## Compilar e executar

Use o Rust estável com o alvo `x86_64-pc-windows-msvc` e as ferramentas de compilação C++ da Microsoft. O executável incorpora o runtime C e, para as leituras de GPU, carrega a NVML fornecida pelo driver NVIDIA. O projeto usa a biblioteca padrão do Rust e as APIs nativas, sem dependências do Cargo.

```powershell
cargo build --release --locked
.\target\release\argos.exe
.\target\release\argos.exe --gpu 0 --samples 5 --interval-ms 1000
```

O comando padrão consulta todas as GPUs NVIDIA enumeradas pela NVML. Cada GPU recebe uma leitura dos sensores básicos e uma tentativa de leitura dos contadores GPM, com 1 s entre as coletas. `--samples` aumenta o número de intervalos GPM por GPU. `--interval-ms` aceita a partir de 101 ms, pois a NVIDIA exige mais de 100 ms entre amostras. [Funções GPM da NVML](https://docs.nvidia.com/deploy/nvml-api/latest/api/group__nvmlGpmFunctions.html)

## Como interpretar as leituras

- O inventário usa as APIs do Windows para identificar o sistema, a CPU, os processadores lógicos ativos e a RAM utilizável.
- A NVML fornece o modelo da GPU, as versões do driver e da biblioteca, o modo do driver, a VRAM, a atividade geral da GPU e da memória, a temperatura, a potência, os clocks e a atividade de NVENC e NVDEC. Cada API tem sua própria janela de medição. Por exemplo, o driver retorna a potência média de 1 s na GPU de referência. [Consultas de dispositivos da NVML](https://docs.nvidia.com/deploy/nvml-api/latest/api/group__nvmlDeviceQueries.html)
- A consulta de suporte GPM é feita por GPU. Quando há suporte, duas amostras alimentam o cálculo dos cinco contadores percentuais abaixo. O tempo impresso é o intervalo entre coletas observado pelo programa. Uma leitura válida de `0%` é exibida como valor. Falhas na consulta, na coleta ou em um contador individual são exibidas com o motivo disponível. [Definições dos contadores GPM](https://docs.nvidia.com/deploy/nvml-api/latest/api/group__nvmlGpmEnums.html)

| ID GPM | Métrica | O que o driver mede |
| --- | --- | --- |
| 2 | Atividade dos SMs | Percentual de SMs ocupados |
| 3 | Ocupação dos SMs | Warps ativos em relação ao máximo teórico |
| 5 | Atividade Tensor | Fração do tempo em que os SMs executaram operações Tensor |
| 10 | Largura de banda da DRAM | Fração da largura de banda teórica utilizada |
| 12 | Atividade FP32 | Fração do tempo em que os SMs executaram operações FP32 fora das unidades Tensor |

## Validação inicial

Na RTX 5060 Ti de 16 GB, com Windows 11 build 26320 e driver NVIDIA 621.14 em modo WDDM, a NVML informou suporte a GPM e reportou 16.311 MiB de VRAM. O executável compilado em modo release, rodando sem elevação, obteve os cinco contadores tanto na janela padrão de 1 s quanto em cinco janelas consecutivas configuradas para 101 ms.

Na janela de 1 s, o driver reportou 11,81% de atividade FP32, 0,31% de atividade Tensor, 62,89% de atividade dos SMs, 29,25% de ocupação dos SMs e 22,65% da largura de banda teórica da DRAM. A execução usou a atividade normal da máquina, sem carga controlada. A precisão sob cargas conhecidas e o impacto nos frametimes ainda precisam ser medidos.

## Reproduzir o diagnóstico

1. Rode a sonda com `--samples 5 --interval-ms 1000` e registre o modelo da GPU, o driver, o modo e os estados dos contadores em repouso.
2. Repita durante uma carga conhecida. Para validar a resposta dos contadores, mantenha a carga estável por vários intervalos e use cargas que exercitem operações FP32 e Tensor em execuções separadas.
3. Se quiser comparar com a ferramenta da NVIDIA, faça uma execução separada com `nvidia-smi dmon -i 0 -c 5 -d 1 --gpm-metrics 2,3,5,10,12 --gpm-options d`, nos drivers que aceitam essa opção. O `nvidia-smi` é opcional para a comparação. [Referência do NVIDIA System Management Interface](https://docs.nvidia.com/deploy/nvidia-smi/index.html)

RT Cores, cache L2, FPS e frametime seguem em investigação. A interface gráfica, o histórico e a medição de impacto no desempenho pertencem às fases posteriores.

## Licença

O código do ARGOS é distribuído sob a [licença MIT](LICENSE).
