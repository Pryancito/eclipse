# Notas para desarrolladores (borrador)

**Español** · [English](for-developers.en.md)

> Traducido del chino desde las notas del zCore original (rCore-OS). Se han
> puesto al día la versión de la cadena de herramientas y la lista de
> submódulos; la lista de características inestables es la de entonces.

Este documento sirve para compartir algunas decisiones de desarrollo y avisar
de cosas que hay que tener en cuenta.

## Índice

- [Política de la cadena de herramientas](#política-de-la-cadena-de-herramientas)
  > - Cuándo se actualiza la cadena de herramientas que se sigue
  > - Qué características inestables se usan y por qué
- [Calidad del código](#calidad-del-código)
  > Cómo usar `#[deny(...)]` y `#[allow(...)]`
- [Política de dependencias](#política-de-dependencias)
  > Cuándo y cómo añadir una dependencia
- [features y cfg](#features-y-cfg)
  > Cómo usar la compilación condicional

## Política de la cadena de herramientas

Por los motivos concretos que se detallan [más abajo](#características-inestables-que-se-usan),
el proyecto depende de algunas características inestables y solo compila con
una cadena nightly. El repositorio guarda la cadena probada en un
[fichero de configuración de toolchain](../rust-toolchain.toml) que cargo
entiende, así que al compilar con cargo se instala sola la cadena buena. Hoy
está fijada a `nightly-2026-09-01`.

Aunque la cadena es nightly, se actualiza siguiendo a la estable: cada vez que
sale una estable nueva, la cadena por defecto pasa a la nightly más cercana a
esa estable, más o menos cada 6 semanas. Basta con mirar en la
[web de versiones](https://forge.rust-lang.org/) cuándo se bifurcó la última
estable y usar la nightly de esa fecha.

### Características inestables que se usan

#### [`doc_cfg`](https://doc.rust-lang.org/unstable-book/language-features/doc-cfg.html)

Módulos: `zcore-drivers`, `kernel-hal`, `zcore-loader`

Marca en la documentación en qué plataformas está disponible cada elemento.

Mejora la documentación pero no afecta al uso; se puede quitar si hace falta.

#### [`naked_functions`](https://doc.rust-lang.org/unstable-book/language-features/naked-functions.html), [`asm_sym`](https://doc.rust-lang.org/unstable-book/language-features/asm-sym.html), [`asm_const`](https://doc.rust-lang.org/unstable-book/language-features/asm-const.html)

Módulo: `zcore`

Las tres sirven para escribir una función desnuda en Rust. Una función desnuda
no inserta operaciones de pila, así que se puede usar antes de preparar la
pila. Junto con `asm_const`, que mete constantes de Rust en el ensamblador en
línea, y `asm_sym`, que mete símbolos de Rust, permite dejar casi toda la fase
de arranque bajo la protección de Rust (sin constantes escritas a mano ni
símbolos globales exportados).

Se puede quitar, pero no se recomienda.

#### [`default_alloc_error_handler`](https://doc.rust-lang.org/unstable-book/language-features/default-alloc-error-handler.html)

Módulo: `zcore`

Pide a `alloc` un callback por defecto para los fallos de asignación.

En cuanto se usan `no_std` y `alloc` a la vez hace falta un callback de fallo
de asignación: o el de por defecto o uno propio mediante otra característica
inestable, [`alloc-error-handler`](https://doc.rust-lang.org/unstable-book/language-features/alloc-error-handler.html).

## Calidad del código

La calidad del código se apoya sobre todo en **clippy**, así que hay que
preparar un entorno en el que clippy se ejecute con facilidad. Tiene tres
partes:

1. Integración con rust-analyzer

   Si desarrollas con VS Code, rust-analyzer tiene la opción checkOnSave, que
   comprueba al guardar (con lo rápidos que son los compiladores de hoy, es
   viable). Con el guardado automático, se comprueba todo el rato; basta con
   poner clippy como comprobación por defecto.

   Claro que clippy es lento; si tu equipo no da para tanto, desactívalo.

2. `#![deny(warnings)]`

   Esta opción trata como errores los avisos del módulo actual (y de sus
   submódulos), lo que impide compilar código con avisos y ayuda a resolver
   los problemas pronto. clippy también genera avisos, que igualmente cuentan
   como errores.

   Hoy todos los módulos tienen esta opción (algunos prohíben más cosas, como
   unsafe_code o missing_docs, que no tratamos aquí) y compilan. Mientras
   desarrollas puedes comentarla por comodidad, pero antes de abrir un PR
   vuelve a ponerla.

3. GitHub Actions

   La comprobación con clippy está en el flujo de trabajo de GitHub y se
   ejecuta en cada commit, lo que demuestra a los demás que la calidad de tu
   código está cubierta. En local puedes comprobarlo con `cargo check-style`,
   que sigue el mismo proceso y usa clippy igual que el flujo de GitHub.

## Política de dependencias

De más particular a más general, las dependencias toman estas 4 formas:

1. [Submódulos de git](#submódulos-de-git)
2. [Proyectos cargo en repositorios personales](#proyectos-cargo-en-repositorios-personales)
3. [Proyectos cargo en repositorios de una organización](#proyectos-cargo-en-repositorios-de-una-organización)
4. [Crates publicados en crates.io](#crates-publicados-en-cratesio)

### Submódulos de git

La forma más particular; es obligada para los proyectos que no usan cargo
(porque no están escritos en Rust).

Hoy dependen así:

- libc-test

  Los tests de musl libc, escritos en C.

- tests

  El framework de tests, sobre todo en Python.

- nvidia-rm-sys/vendor/open-gpu-kernel-modules

  Los módulos de kernel abiertos de NVIDIA, de donde sale el RM.

(`rboot`, el cargador UEFI, era un submódulo y hoy vive dentro del
repositorio.)

### Proyectos cargo en repositorios personales

Una forma bastante particular. Un proyecto puede depender directamente de un
proyecto personal si y solo si se da alguno de estos casos:

- es experimental, inestable o incluso puede abandonarse;
- es estable, pero no encaja en crates.io y ninguna organización quiere
  acogerlo;
- es un fork de un proyecto de una organización con un PR enviado pero aún no
  aceptado;
- es un fork de un proyecto de una organización que, por buenos motivos, se
  ha decidido mantener separado para siempre como proyecto independiente.

Si es posible, estos casos hay que resolverlos activamente.

Hay que fijar el hash del commit.

### Proyectos cargo en repositorios de una organización

La forma normal, aunque si es posible es mejor publicarlos en crates.io.

Hay que fijar el hash del commit.

### Crates publicados en crates.io

La forma normal; se pueden usar sin más.

Intenta seguir la última versión. Si una dependencia no se puede seguir, deja
anotado el motivo.

## features y cfg

zCore admite hardware distinto en varias plataformas, así que es difícil no
usar opciones de compilación. Pero usarlas mal puede traer confusión o reducir
la cobertura de los tests. Por eso se recomienda seguir estas reglas al poner
`#[cfg(...)]`:

1. Lo que depende solo de la plataforma usa target_arch;
2. lo que depende de otros factores, mejor decidirlo en tiempo de ejecución,
   por ejemplo con el device tree;
3. lo que no se puede decidir en tiempo de ejecución puede usar una feature,
   pero hay que explicar el motivo en el punto de entrada del proyecto
   (lib.rs o main.rs);
4. una feature ligada a una plataforma debe llevar una restricción como esta,
   en vez de usar `all(target_arch = ..., feature = ...)`:

   ```rust
   #[cfg(all(feature = "sbi", not(target_arch = "riscv64")))]
   compile_error!("`sbi` is only available on RISC-V platforms");
   ```

> El código actual no cumple del todo estas reglas; se irá corrigiendo.
