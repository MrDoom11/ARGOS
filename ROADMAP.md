# Roadmap do ARGOS

O objetivo é monitorar o hardware em uma janela leve para o segundo monitor, com leituras confiáveis e impacto mensurável no desempenho. O alvo inicial é o Windows 11, com a RTX 5060 Ti como GPU de referência.

## 1. Sonda e descoberta de sensores · concluída

O ARGOS 0.1.0 identifica o sistema e lê os sensores básicos da NVIDIA. Na GPU de referência, a NVML retornou os cinco contadores GPM em modo WDDM, com uma conta de usuário padrão. A sonda continua sendo a ferramenta de diagnóstico de disponibilidade. Os resultados e as condições do teste estão no [README](README.md).

## 2. Coleta contínua e histórico · concluída

O modo `watch` acompanha temperatura, potência e VRAM de uma GPU junto aos demais sensores NVML, aos contadores GPM e ao uso de CPU e RAM. Os grupos têm intervalos independentes. O padrão é 1 s, com histórico de até 5 minutos em memória. Cada leitura registra o instante e o estado. O comando padrão continua disponível para diagnósticos pontuais.

- O histórico preserva lacunas e distingue indisponibilidade de um `0` medido.
- Sensores atendidos pela mesma chamada ao driver compartilham a coleta. O agendamento respeita o intervalo mínimo do GPM.

**Validação.** Os testes simulados cobriram agendamento, limite do histórico, lacunas e recuperação após uma falha. A sessão na máquina de referência durou 30 min e 5 s, reteve 298 amostras por grupo na janela final e encerrou normalmente. O encerramento por `Ctrl+C` também passou num teste separado. Os números de CPU e memória do modo console estão no [README](README.md#coleta-contínua).

## 3. Janela de monitoramento · em validação

A janela nativa e redimensionável usa o histórico do motor e dá prioridade ao traçado. O painel permite escolher os sensores, as linhas e colunas da grade, mesclar curvas da mesma unidade e configurar o período. Essas preferências são salvas localmente.

**Critério para avançar.** Verificar em segundo monitor que o redimensionamento e as pausas na interface preservam o ritmo da coleta. Os gráficos devem representar corretamente os horários e as lacunas nas leituras. Medir o custo da janela comparando com o modo console.

## 4. Precisão e desempenho

Usar a medição inicial para definir limites de consumo. Repetir os testes em repouso e sob cargas conhecidas, incluindo FP32 e Tensor em sessões separadas. Comparar com ferramentas de referência usando unidades e janelas equivalentes. Medir CPU, RAM e atividade de disco e GPU do ARGOS. Para jogos, usar uma ferramenta externa para medir a mesma cena com e sem o ARGOS e comparar os percentis 95 e 99 dos tempos de quadro.

**Critério para avançar.** Documentar o protocolo, a variação entre execuções e as diferenças em relação às referências. Os resultados precisam sustentar as metas de consumo e impacto definidas para a versão.

## 5. Investigações e expansão

Tratar RT Cores, cache L2, FPS, frametime e novos sensores de sistema como investigações independentes. Cada investigação deve registrar o que a API mede, em quais configurações funciona, quais permissões requer e quanto custa coletar. Só incorporar uma métrica quando houver evidência de disponibilidade, significado e custo aceitável.

## Como manter o foco

- Trabalhar com uma etapa ativa. Guardar ideias novas na fila da etapa correspondente.
- Definir para cada tarefa uma entrega pequena e uma forma de conferir o resultado. Registrar a fonte, a unidade e o significado de todo sensor novo.
- Manter as leituras no computador por padrão. Revisar os dados de teste antes de publicá-los e usar apenas o resumo técnico necessário.
