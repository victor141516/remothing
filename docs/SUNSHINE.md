# Sunshine frente a remothing: qué hace por rendimiento y qué podríamos adoptar

Informe de análisis. No se ha implementado nada de lo que propone.

## Alcance y método

- Código analizado: `LizardByte/Sunshine`, commit `ec2676b` (2026-11-10), clonado en
  `.tools/ref/Sunshine` (fuera del control de versiones). Se han leído `src/video.cpp`,
  `src/stream.cpp`, `src/input.cpp`, `src/audio.cpp`, `src/nvenc/*`,
  `src/platform/windows/*`, `src/platform/linux/*`, `src/config.cpp` y
  `docs/configuration.md`.
- Sunshine no se ha compilado ni ejecutado: todo lo que se dice de él sale de leer el
  código. Las referencias `fichero:línea` son relativas a la raíz de Sunshine; las
  marcadas con `~` pueden desviarse unas líneas.
- De remothing se ha usado el código actual y `docs/ARCHITECTURE.md` / `docs/RESULTS.md`.
- **Los beneficios que se citan para remothing son esperados, no medidos**, salvo
  donde se dice lo contrario. Cada propuesta indica cómo medirla con nuestro arnés.

## Resumen

Sunshine y remothing resuelven problemas distintos. Sunshine envía **vídeo a pantalla
completa, a tasa fija, por UDP con FEC**, a un cliente nativo (Moonlight), pensado para
juegos en red local. remothing envía **teselas sin pérdida más una capa de vídeo, a la
tasa que el enlace admite**, a un navegador. Por eso buena parte de lo que hace Sunshine
no nos aplica, y en varios puntos vamos por delante (control de congestión, captura por
regiones, nada de tráfico con la pantalla quieta).

Donde Sunshine sí nos saca ventaja es en tres frentes:

1. **Planificación en el host.** Sunshine sube la prioridad de sus hilos, de su proceso y
   de su trabajo en la GPU, fija la resolución del temporizador y ajusta el controlador de
   NVIDIA. Nosotros no hacemos nada de esto. Es barato y ataca justo lo que nos queda por
   mejorar: el p99 cuando el equipo está ocupado.
2. **Control fino del codificador.** Su envoltorio propio de NVENC le da invalidación de
   fotogramas de referencia, refresco intra y codificación en dos motores. A nosotros nos
   daría además cambiar el bitrate sin fotograma clave, que hoy nos cuesta dos o tres
   fotogramas clave en cada arranque. También ofrece HEVC y AV1; nosotros solo H.264.
3. **Tolerancia a pérdidas sin retransmisión.** Con FEC sobre UDP, un paquete perdido no
   retrasa nada. Nosotros dependemos de la retransmisión de TCP o SCTP, y es nuestro peor
   caso medido (Wi-Fi con 2 % de pérdida). Adoptarlo es un cambio grande.

### Propuestas, por orden de relación beneficio/coste

| # | Mejora | Qué aportaría | Coste | Riesgo |
|---|---|---|---|---|
| 1 | Prioridad de hilos y de proceso; resolución de temporizador propia | Menos picos de latencia con la CPU ocupada; ritmo de envío que no dependa de otros programas | Bajo | Bajo |
| 2 | Prioridad de GPU y `SetMaximumFrameLatency(1)` | Captura y codificación estables con la GPU cargada (juego, vídeo, el propio navegador) | Bajo | Medio: fallo conocido del controlador NVIDIA con HAGS |
| 3 | Perfil NVIDIA «máximo rendimiento» para el proceso | Menor tiempo de codificación con la GPU casi parada, que es nuestro caso habitual | Medio (NvAPI) | Bajo; más consumo |
| 4 | `DwmEnableMMCSS` mientras haya clientes | Composición de Windows más regular bajo carga | Muy bajo | Bajo |
| 5 | NVENC nativo: cambio de bitrate sin fotograma clave | Quita los 2–3 fotogramas clave del arranque y el de cada cambio de límite | Alto | Medio |
| 6 | HEVC / AV1 opcionales, con codificación repartida | Menos bitrate a igual calidad en enlaces lentos; menos tiempo de codificación en 4K | Medio-alto | Medio: soporte del navegador |
| 7 | Ensayo de control de tasa: VBV de un fotograma, doble pasada a cuarto de resolución | Tamaño de fotograma más acotado, menos fotogramas tardíos en enlaces lentos | Bajo (es un ensayo) | Bajo |
| 8 | QoS: DSCP en nuestros sockets y modo «streaming» de la Wi-Fi del host | Menos fluctuación en Wi-Fi local | Bajo-medio | Bajo; algún router lo trata mal |
| 9 | AMD e Intel: límite de bitrate con las recetas de Sunshine | En esas GPU el vídeo seguiría al enlace; hoy va a calidad constante | Medio | Medio: no tenemos ese hardware para probar |
| 10 | Vídeo por datagramas con FEC (sin bloqueo de cabeza de línea) | Latencia estable con pérdidas | Muy alto | Alto |
| 11 | Linux con escritorio real: NvFBC / KMS / PipeWire | Captura sin pasar por la CPU; soporte de Wayland | Muy alto | Medio |

Mi recomendación es hacer 1, 2 y 4 juntas (son pequeñas y se miden con una sola
batería), validar 3 y 7 a mano antes de escribir código, y decidir 5 y 6 con esos
números delante. 9 necesita una máquina con GPU AMD o Intel. 10 y 11 son proyectos aparte.

## Diferencias de fondo

| Aspecto | Sunshine | remothing |
|---|---|---|
| Qué se envía | Cada fotograma entero como vídeo | Teselas 64×64 sin pérdida, y vídeo solo para lo que se mueve |
| Tasa | Bitrate fijo acordado al conectar; no cambia durante la sesión | Estimación de enlace continua; el límite del codificador la sigue |
| Pantalla quieta | Reenvía duplicados a la mitad de los fps del cliente | No envía nada |
| Transporte | UDP propio, RTP + Reed-Solomon, sin retransmisión | WebSocket (TCP) o canal de datos WebRTC (SCTP fiable) |
| Ante una pérdida | FEC; si no basta, invalidar referencias; si no, fotograma clave | La retransmite el transporte; todo lo posterior espera |
| Control de congestión | Ninguno: las estadísticas de pérdida del cliente solo se registran | Propio, tipo BBR, por ACK de fotograma |
| Regiones cambiadas | No las usa: copia y codifica el fotograma entero | Rectángulos sucios y movidos de DXGI / XDamage |
| Cliente | Nativo (Moonlight) | Navegador (WebCodecs + WebGL2) |
| Códecs | H.264, HEVC, AV1; 4:2:0 y 4:4:4; HDR | H.264 4:2:0 (la nitidez la da la capa sin pérdida) |

Evidencia en Sunshine: bitrate fijo y ausencia de reconfiguración (`src/rtsp.cpp:1202-1226`,
búsqueda de `reconfigure` sin resultados); estadísticas de pérdida solo registradas
(`src/stream.cpp:~1208-1222`); duplicados (`src/video.cpp:2437-2440, 2478-2490`); sin
rectángulos sucios (`GetFrameDirtyRects` no aparece en `src/`).

## Propuestas en detalle

### 1. Prioridad de hilos y proceso, y temporizador propio

**Qué hace Sunshine.**
- Hilo de captura y de control con `THREAD_PRIORITY_HIGHEST`; codificación y envío con
  `THREAD_PRIORITY_ABOVE_NORMAL` (`src/platform/windows/misc.cpp:1149-1175`,
  `src/video.cpp:1703, 2905`, `src/stream.cpp:1337, 1620`).
- Mientras hay una sesión, el proceso pasa a `HIGH_PRIORITY_CLASS` y vuelve a normal al
  acabar (`misc.cpp:1227-1228, 1303-1304`).
- Fija la resolución del temporizador del sistema a 0,5 ms con `NtSetTimerResolution`, y
  si falla usa `timeBeginPeriod(1)` (`misc.cpp:110-126, 1218-1225`).
- En Linux pide *nice* −15 y −10 a RealtimeKit, y si no puede usa `setpriority`
  (`src/platform/linux/misc.cpp:523-586`). No usa `SCHED_FIFO`.

**Qué tenemos.** Ningún hilo ni el proceso cambian de prioridad (no hay ninguna llamada
de este tipo en `crates/`). El hilo que construye y dosifica los fotogramas de cada
cliente espera con `std::thread::park_timeout` (`crates/core/src/stream.rs:614`), que en
Windows depende de la resolución del temporizador del sistema. Solo la captura usa un
temporizador de alta resolución propio (`crates/platform-windows/src/dxgi.rs:507-521`).

**Medido hoy en este PC.** El temporizador del sistema está a 0,75 ms porque algún otro
programa lo tiene pedido, y con eso las esperas de 1 a 5 ms se pasan como mucho 0,8 ms.
Es decir, ahora mismo no perdemos nada, pero por suerte: en un equipo donde nadie lo
pida, el valor por defecto es 15,6 ms y nuestro dosificador despertaría tarde.

**Propuesta.**
- Prioridad alta para el hilo de captura; por encima de lo normal para los hilos de
  cliente, el de WebRTC y los de red.
- Pedir nosotros la resolución del temporizador mientras haya clientes conectados.
- Clase de prioridad alta del proceso solo mientras haya clientes, como Sunshine.
- En Linux, *nice* negativo; en el contenedor hace falta `--cap-add SYS_NICE`.

**Cómo medirlo.** La batería actual (`box`, `scroll`, `full`) con la CPU del host cargada
a propósito (una compilación en todos los núcleos) y comparar `srv wait` y
`capture→draw` p99 antes y después. Para el temporizador: repetir con el navegador en
otra máquina y sin nada más abierto en el host.

### 2. Prioridad de GPU

**Qué hace Sunshine.**
- `D3DKMTSetProcessSchedulingPriorityClass(REALTIME)` para el proceso, tras activar el
  privilegio `SE_INC_BASE_PRIORITY_NAME`. En NVIDIA con planificación de GPU acelerada por
  hardware (HAGS) y la opción `nvenc_realtime_hags` desactivada baja a `HIGH`, porque
  «realtime» puede congelar la codificación o tumbar el controlador
  (`src/platform/windows/display_base.cpp:~629-708`).
- `IDXGIDevice::SetGPUThreadPriority(7)` en el dispositivo de captura y en el de
  codificación (`display_base.cpp:~718`, `display_vram.cpp:900`).
- `IDXGIDevice1::SetMaximumFrameLatency(1)` en el de captura (`display_base.cpp:~724-736`).
- En Linux crea el contexto EGL con `EGL_CONTEXT_PRIORITY_HIGH_IMG`
  (`src/platform/linux/graphics.cpp:601-606`).

**Qué tenemos.** Nada en el servidor. El banco `d3d11_bench` ya acepta `--gpu-priority`,
pero no hay resultados guardados en `docs/RESULTS.md`.

**Propuesta.** Prioridad 7 en nuestro dispositivo D3D11 (compartido por captura y
codificador) y clase de planificación `HIGH` del proceso; `REALTIME` solo como opción,
por el fallo que documenta Sunshine. Requiere proceso elevado: el servicio (LocalSystem)
puede; la instancia sin elevar se quedaría como está y lo diría en el registro.

**Cómo medirlo.** `encode` y `capture→draw` p99 con la GPU cargada: un juego o un vídeo
4K en el otro monitor. Sin carga de GPU no espero diferencia.

### 3. Perfil del controlador NVIDIA

**Qué hace Sunshine.** Crea un perfil de aplicación en el controlador con
`PREFERRED_PSTATE = PREFER_MAX` (`src/platform/windows/nvprefs/driver_settings.cpp:267-302`,
opción `nvenc_latency_over_power`, activa por defecto). Su documentación dice que el
algoritmo adaptativo de estados de energía de NVIDIA «no funciona bien con streaming de
baja latencia» y que desactivar la opción «puede aumentar notablemente la latencia de
codificación». También fuerza que OpenGL y Vulkan presenten sobre DXGI para poder
capturarlos a toda velocidad (`driver_settings.cpp:~153-208`); eso solo importa para
juegos a pantalla completa.

**Por qué nos importa.** Es el caso más parecido al nuestro: en un escritorio la GPU está
casi parada, que es cuando el controlador baja la frecuencia. Nuestros tiempos de
codificación en Windows son 2,4–3,5 ms p50 y 4–6 ms p99 (`docs/RESULTS.md`).

**Propuesta.** Antes de escribir nada: poner a mano «Modo de control de energía: máximo
rendimiento» para `remothing.exe` en el panel de NVIDIA y comparar `encode` p50/p99 en el
escenario `box`. Si mejora, crear el perfil desde el servidor con NvAPI y deshacerlo al
salir, como Sunshine.

### 4. MMCSS para el compositor

**Qué hace Sunshine.** `DwmEnableMMCSS(true)` al empezar a transmitir y `false` al
terminar (`src/platform/windows/misc.cpp:1215-1216, 1316-1317`).

**Por qué nos importa.** En Windows, lo que va de que la aplicación pinta a que la
duplicación entrega el fotograma (5–10 ms, `docs/RESULTS.md`) es trabajo del compositor.
No lo controlamos, pero sí podemos pedir que se planifique como multimedia.

**Cómo medirlo.** `app→draw` menos `capture→draw`, con la CPU cargada. Es una llamada;
si no se nota, se quita.

### 5. NVENC nativo

**Qué hace Sunshine.** En Windows no usa FFmpeg para NVENC, sino un envoltorio propio del
SDK (`src/nvenc/`). El código no explica por qué, pero sí muestra qué consigue con él:

- **Invalidación de referencias.** Cuando el cliente avisa de fotogramas perdidos, llama a
  `nvEncInvalidateRefFrames` para ese rango y el siguiente fotograma P se apoya en una
  referencia anterior buena. Solo si el rango no cabe en el búfer de referencias recurre
  a un fotograma clave (`src/nvenc/nvenc_base.cpp:799-834`).
- Para que eso funcione mantiene 5 referencias guardadas pero cada fotograma usa solo una
  (`nvenc_base.cpp:284-299`).
- **Refresco intra** en lugar de fotogramas clave, a petición del cliente
  (`nvenc_base.cpp:329-348`).
- **Codificación repartida** entre los dos motores NVENC de la tarjeta, solo HEVC y AV1;
  la documentación dice que «reduce notablemente la latencia de codificación»
  (`nvenc_base.cpp:226-247`).
- Espera acotada: codificación asíncrona con un evento y 100 ms de tope
  (`nvenc_base.cpp:744-761`).
- Cierre del codificador en un hilo aparte para no bloquear el reinicio
  (`src/video.cpp:2415-2431`).

**Qué tenemos.** NVENC a través de FFmpeg. Cambiar el límite de bitrate fuerza un
fotograma clave (`docs/ARCHITECTURE.md`, «cap changes cost an IDR with NVENC»). Con el
arranque lento nuevo eso se nota: en cada conexión hay dos o tres cambios de límite en el
primer segundo, cada uno con su fotograma clave. En la batería de hoy, enlace de
50 Mbit/s: 69 KB, luego 143 KB, luego 387 KB.

**Propuesta.** Un envoltorio propio de NVENC nos daría, por este orden de interés:

1. **Cambio de bitrate sin fotograma clave** con `nvEncReconfigureEncoder`. Esto Sunshine
   no lo hace (su bitrate es fijo); es lo que el camino nativo nos permitiría a nosotros.
   Que el SDK lo admita sin reiniciar el codificador sale de la documentación de NVIDIA,
   no lo he probado.
2. Invalidación de referencias cuando se abandona el canal de datos con fotogramas de
   vídeo sin entregar, en vez del fotograma clave actual.
3. Codificación repartida, si además adoptamos HEVC o AV1.

**Coste y riesgo.** Alto: es código `unsafe` contra el SDK, con selección de versión en
tiempo de ejecución (Sunshine compila contra cuatro versiones del SDK,
`src/nvenc/nvenc_dynamic_factory.cpp:66-113`), y hay que mantener el camino de FFmpeg
para AMD, Intel y x264. Solo compensa si el punto 1 mejora de verdad el arranque.

**Cómo medirlo.** Antes de implementar: contar en el registro de eventos cuántos
fotogramas clave por cambio de límite hay en una sesión real y cuántos fotogramas tardíos
les siguen. Después: la batería de arranque (primeros 2,5 s) en los perfiles 18 Mbit/s y
móvil.

### 6. HEVC y AV1

**Qué hace Sunshine.** Ofrece H.264, HEVC y AV1, y prueba cada uno al arrancar con una
codificación real (`src/video.cpp:3029-3464`).

**Qué tenemos.** Solo H.264: el cliente configura `avc1.640033`
(`client/src/worker.ts:408`).

**Propuesta.** Códec negociado: el cliente pregunta a WebCodecs qué decodifica por
hardware y el servidor elige. Con el mismo bitrate, HEVC y AV1 dan más calidad, que es
justo lo que falta en el perfil móvil (6 Mbit/s). Con HEVC o AV1 se podría activar además
la codificación repartida de NVENC.

**Riesgos.** Hay que comprobar tres cosas que no he verificado: qué navegadores
decodifican HEVC por hardware con `optimizeForLatency`, si la latencia de decodificación
es igual que con H.264, y si nuestra versión de FFmpeg expone la codificación repartida.
La marca que usamos para que el navegador no reordene fotogramas es específica de H.264 y
habría que buscar la equivalente.

**Cómo medirlo.** Misma batería con `decode` y `capture→draw`, más una comparación de
calidad a bitrate fijo en el perfil móvil.

### 7. Control de tasa: ensayo

**Qué hace Sunshine.** Tasa constante con un búfer VBV de **un solo fotograma**
(`bitrate / fps`), y por defecto doble pasada a cuarto de resolución
(`src/nvenc/nvenc_base.cpp:251-274`, `src/nvenc/nvenc_config.h:36`). Su documentación
explica el motivo: sin doble pasada hay «excesos ocasionales de bitrate y la consiguiente
pérdida de paquetes». Para ellos un fotograma demasiado grande es pérdida; para nosotros
es cola.

**Qué tenemos.** Tasa variable con límite, QP mínimo 20, VBV de dos fotogramas a 60 fps
(33 ms de enlace por fotograma), sin doble pasada
(`crates/encoder/src/ffmpeg.rs:48-66, 160-172`). El comentario del código dice que la
doble pasada no cambia el tiempo de codificación; no dice nada del tamaño de los
fotogramas, que es lo que aquí interesa.

**Propuesta.** Un ensayo, sin compromiso: VBV de un fotograma y `multipass=qres`, y mirar
la distribución del tamaño de fotograma y los fotogramas tardíos en los perfiles de
18 Mbit/s y móvil con el escenario `full`. En el perfil móvil hoy vemos fotogramas que
llegan 217–455 ms tarde.

### 8. Calidad de servicio en la red

**Qué hace Sunshine.**
- Marca el tráfico: en Linux `IP_TOS` con DSCP CS5 para vídeo y `SO_PRIORITY` 5
  (`src/platform/linux/misc.cpp:1061-1122`); en Windows con qWAVE,
  `QOSAddSocketToFlow(..., QOSTrafficTypeAudioVideo)`
  (`src/platform/windows/misc.cpp:~1678-1775`). Opción `dscp_tagging`, activa por defecto,
  porque «algunos routers tratan mal el tráfico marcado».
- Pone las interfaces Wi-Fi del host en modo «media streaming» para evitar los barridos
  de fondo que causan pérdida y fluctuación (`misc.cpp:~1240-1268`).
- Búfer de envío UDP de 1 MB (`src/stream.cpp:~2062-2067`).

**Qué tenemos.** Solo `TCP_NODELAY` (`crates/core/src/app.rs:174`). Los sockets UDP de
WebRTC usan los valores por defecto del sistema.

**Propuesta.** Marcar DSCP en los sockets UDP de WebRTC y en los TCP del flujo, y activar
el modo streaming de la Wi-Fi si el host está conectado por Wi-Fi. El efecto se limita a
la red local: en Internet las marcas suelen borrarse y a través de un proxy solo llegan al
primer salto. Tiene sentido sobre todo con el cliente por Wi-Fi en casa.

**Cómo medirlo.** No se puede con `netem` en bucle local; hace falta una Wi-Fi real con
tráfico de fondo, comparando `net` p99.

### 9. AMD e Intel: límite de bitrate

**Qué hace Sunshine.** Tiene juegos de opciones de baja latencia con tasa limitada para
los dos fabricantes:
- AMF: `usage=ultralowlatency`, `rc=vbr_latency`, `async_depth=1`, sin relleno ni salto de
  fotogramas (`src/video.cpp:979-1087`, valores por defecto en `src/config.cpp:762-777`).
- QuickSync: `low_delay_brc=1`, `async_depth=1`, `low_power=1` con reintento sin él, y
  tasa variable con el máximo igual al objetivo (`src/video.cpp:865-973`).
- VAAPI en Linux: tasa variable con VBV de un fotograma en Intel, constante en el resto
  (`src/platform/linux/vaapi.cpp:339-395`).

**Qué tenemos.** Con AMF y QuickSync codificamos a QP constante 20, sin límite de
bitrate: el código lo dice, «their capped modes are untested here»
(`crates/encoder/src/lib.rs:56-59`). En esas GPU la capa de vídeo no sigue la estimación
del enlace, así que en un enlace lento el vídeo puede pedir más de lo que cabe. No
tenemos backend VAAPI.

**Propuesta.** Adoptar esas opciones y que `caps_bitrate` valga también para AMF y
QuickSync. No se puede validar aquí: hace falta una máquina con esa GPU.

### 10. Vídeo que no espera retransmisiones

**Qué hace Sunshine.** Cada fotograma se parte en paquetes y se le añade paridad
Reed-Solomon: 20 % por defecto, con un mínimo de paquetes de paridad que pide el cliente
y hasta cuatro bloques por fotograma (`src/stream.cpp:808-953, 1697-1739`). Los paquetes
de datos no se copian para calcular la paridad (`stream.cpp:~886-933`). Si el FEC no
basta, el cliente pide invalidar referencias o un fotograma clave. Nunca se retransmite,
así que una pérdida no retrasa los fotogramas siguientes.

**Qué tenemos.** Transportes fiables. Una pérdida detiene todo lo que va detrás hasta que
se retransmite. Es nuestro peor caso medido: con 2 % de pérdida en Wi-Fi, el escenario
`full` baja a 96 fps con 146 ms de p99. Ya probamos entrega no fiable con reenvío propio
sobre el canal de datos y la descartamos, porque el control de congestión de SCTP cuenta
como pérdida también lo que se abandona (`docs/ARCHITECTURE.md`).

**Qué habría que hacer.** No es adoptar una técnica, es un tercer transporte. La vía que
veo es WebTransport: datagramas sobre QUIC, donde el control de congestión del envío lo
pone nuestro servidor y podría ser nuestro propio modelo de enlace. El vídeo iría por
datagramas con FEC y la capa sin pérdida por un flujo fiable. Esto es una idea mía a
partir de lo que hace Sunshine, no algo que Sunshine haga en navegadores.

**Coste y riesgo.** Muy alto. Necesita HTTP/3 con certificado válido, solo funciona con
ruta UDP directa (no a través del proxy ni de Funnel, igual que el canal de datos), el
soporte en Safari está por comprobar, y obliga a tener la invalidación de referencias del
punto 5. Lo dejaría como línea de investigación para cuando las pérdidas sean el problema
principal.

### 11. Linux con escritorio real

**Qué hace Sunshine.**
- **KMS**: toma el framebuffer del plano como DMA-BUF, lo importa como imagen EGL,
  convierte a YUV con un shader y escribe directamente en la superficie del codificador
  (VAAPI) o en memoria compartida con CUDA. Los píxeles no pasan por la CPU
  (`src/platform/linux/kmsgrab.cpp:~2130-2166`, `graphics.cpp:773-793, 1494-1559`,
  `vaapi.cpp:420-466`, `cuda.cpp:1045-1120`). Necesita `CAP_SYS_ADMIN`, que confina a un
  hilo (`kmsgrab.cpp:52-200`).
- **NvFBC** en X11 con NVIDIA: captura directa a CUDA (`cuda.cpp:1583-1905`).
- **PipeWire** y **wlroots** para Wayland, también con DMA-BUF.
- Codificador **VAAPI** con punto de entrada de bajo consumo, para Intel y AMD
  (`vaapi.cpp:195-199, 285-290`).

**Qué tenemos.** X11 con XShm y XDamage, leyendo solo lo que cambia, y subida parcial a
CUDA. Wayland no está soportado y no hay backend VAAPI.

**Valoración.** En X11 nuestra captura es mejor que la de Sunshine, que copia el
fotograma entero en cada ciclo y no usa XDamage (`x11grab.cpp:840-853`). Lo que nos
falta es el camino sin CPU para vídeo a pantalla completa en un escritorio real, y
Wayland. Para el uso actual (contenedores con Xvfb, que no tienen salida de GPU) no
aporta nada. Es un proyecto grande: requeriría además llevar a Linux la comparación de
teselas en GPU que ya tenemos en Windows.

### Menores

- **Marca de tiempo de captura.** Sunshine usa el instante de presentación que da el
  sistema (`LastPresentTime`, `src/platform/windows/display_vram.cpp:1386-1390`). Nosotros
  tomamos la hora al recoger el fotograma (`crates/platform-windows/src/dxgi.rs:590`), así
  que nuestra latencia medida no incluye la espera hasta la recogida (hasta 250 µs de
  sondeo). No cambia el rendimiento, solo hace la medida más honesta.
- **Fusión de entrada atrasada.** Si el hilo de entrada va con retraso, Sunshine fusiona
  los movimientos de ratón y el scroll pendientes antes de aplicarlos
  (`src/input.cpp:1840-2121`). Nosotros aplicamos cada mensaje. Solo se notaría tras un
  corte de red, cuando llegan muchos de golpe.
- **Pantalla despierta.** `SetThreadExecutionState(ES_DISPLAY_REQUIRED)` mientras captura
  (`display_base.cpp:254-261`), para que la pantalla no se apague a mitad de sesión.
- **Captura de reserva con Windows.Graphics.Capture** cuando falla la duplicación
  (`display_wgc.cpp`). Es robustez, no velocidad.

## Lo que no conviene copiar

- **Duplicados con la pantalla quieta.** Existen porque Moonlight espera un flujo
  constante. Nosotros no enviamos nada, y es una ventaja.
- **Dosificador fijo.** Limita el envío al 80 % de 1 Gbit/s, sin relación con el enlace
  real (`src/stream.cpp:~1742-1743`). El nuestro se basa en la estimación.
- **Sin control de congestión.** En enlaces lentos o variables Sunshine depende de que el
  usuario elija bien el bitrate.
- **Copia del fotograma entero.** En el camino por CPU hace una copia GPU→staging y un
  `memcpy` completo por fotograma (`display_ram.cpp:305-337`). Nosotros leemos solo las
  teselas cambiadas.
- **Captura por sondeo en Linux.** KMS, X11 y NvFBC entregan un fotograma en cada ciclo
  aunque no haya cambiado nada.
- **Cola que se vacía entera al desbordarse.** Si el envío se atrasa 32 fotogramas, los
  tira todos (`src/thread_safe.h:409-441`).
- **4:4:4.** Lo ofrecen para que el texto se vea nítido. A nosotros nos lo da la capa sin
  pérdida.
- **Envío por lotes (USO / GSO).** Ahorra CPU a cientos de Mbit/s por UDP. Nuestro vídeo
  va a 5–30 Mbit/s y casi siempre por TCP, donde el sistema ya lo hace.

## Lo que ya hacemos igual o mejor

| Técnica | Sunshine | remothing |
|---|---|---|
| Sin fotogramas B, GOP infinito, clave bajo demanda | Sí | Sí |
| NVENC P1 + ultra baja latencia, sin *lookahead* | Sí | Sí |
| Entrada de codificador sin copia a CPU | Shader a la textura del codificador | Copia GPU→GPU solo de las regiones cambiadas |
| Dispositivo D3D11 libre mientras se espera un fotograma | A medias: espera hasta 200 ms dentro de `AcquireNextFrame` y cede 10 ms cuando la pantalla está quieta | Sí: sondea sin espera cada 250 µs |
| Temporizador de alta resolución en la captura | Sí | Sí |
| Repetición de tecla en el host y liberar todo al desconectar | Sí | Sí |
| Probar los codificadores al arrancar | Sí, con una codificación real | Sí |
| Solo el fotograma más nuevo si el consumidor va lento | Buzón de un hueco | Los cambios pendientes se funden en un fotograma |
| Adaptación al enlace | No | Sí |
| Solo lo que cambia | No | Sí |

## Orden de trabajo sugerido

1. Medir la línea base con carga: CPU ocupada, GPU ocupada, y host sin nada más abierto.
   Hoy no tenemos esos números y son los que deciden 1, 2 y 4.
2. Implementar 1, 2 y 4 y repetir.
3. Ensayos manuales de 3 (panel de NVIDIA) y 7 (opciones del codificador).
4. Contar fotogramas clave por cambio de límite en sesiones reales, para decidir 5.
5. Comprobar soporte y latencia de HEVC/AV1 en los navegadores que usamos, para decidir 6.
