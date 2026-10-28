# Entorno de desarrollo

[English](environment.md)

Excalibur contiene actualmente una biblioteca Rust de proveedores. El agente, la CLI, el servidor Axum y el frontend están previstos; todavía no existe una aplicación ni un servidor de desarrollo que iniciar. Las comprobaciones de dependencias de esta página no son funcionalidades implementadas del producto.

## Entorno comprobado en esta VPS

- Ubuntu 24.04, x86-64; Rust estable 1.99.1 y Cargo 1.99.1 mediante rustup, con rustfmt y Clippy; GCC 13.3 y pkg-config 1.8.1.
- Node.js 24.22.1, npm 11.19.1 y pnpm 10.34.9 están instalados en `/opt/excalibur-toolchain/node24`. El archivo de Node se verificó con la suma SHA-256 publicada. El `/usr/bin/node` del sistema permanece en la versión 18, demasiado antigua para Vite actual.
- Para usar la instalación aislada en una terminal de esta VPS:

  ```sh
  export PATH="/opt/excalibur-toolchain/node24/bin:$PATH"
  node --version
  pnpm --version
  ```

La ruta `/opt` es propia de esta máquina. En otra, instala una versión LTS compatible de Node.js y pnpm siguiendo sus instrucciones oficiales. Vite actual requiere Node 20.19+ o 22.12+; elige una versión LTS con soporte ([Vite](https://vite.dev/guide/), [versiones de Node](https://nodejs.org/en/about/previous-releases), [pnpm](https://pnpm.io/installation)).

## Comprobación de dependencias

Un proyecto Rust temporal compiló Axum 0.8.9 y Tokio 1.54.1 con este entorno. Axum aún no es dependencia de Excalibur; Tokio se utiliza en las pruebas de la biblioteca de proveedores. Otro proyecto temporal con pnpm instaló e importó React 19.3.1, React DOM 19.3.1, Vite 8.3.5, su plugin React 6.1.2, Tailwind CSS y su plugin Vite 4.3.3, y TypeScript 7.0.2. `vite --version` y `tsc --version` funcionaron. Son versiones observadas en la comprobación temporal, no dependencias fijadas ni instaladas en este repositorio; cuando se cree el frontend habrá que elegir y fijar versiones compatibles.

## Comprobaciones Rust existentes

Desde la raíz del repositorio:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Las pruebas de lógica, datos y permisos se realizan en backend, con respuestas simuladas para proveedores. Cuando exista frontend, los cambios solo visuales usan su servidor de desarrollo y revisión visual en pantallas pequeñas y grandes; no se ejecuta un build de producción para cada cambio exclusivamente visual ni se añaden suites funcionales de frontend. Para otros cambios se mantienen las comprobaciones de compilación, tipos y lint pertinentes. Por ahora no hay comandos `pnpm install` o `pnpm dev` aplicables a este repositorio.
