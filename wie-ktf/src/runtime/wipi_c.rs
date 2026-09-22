use jvm::Jvm;
use wie_backend::System;
use wie_core_arm::{ArmCore, ResultWriter, SvcId};
use wie_util::{Result, WieError};

use crate::runtime::SVC_CATEGORY_WIPIC;
use crate::runtime::svc_ids::{WIPICKernelMethodId, WIPICTableId};

mod context;
pub mod interface;
mod method_table;

use context::KtfWIPICContext;

async fn handle_wipic_svc(core: &mut ArmCore, (system, jvm): &mut (System, Jvm), id: SvcId) -> Result<()> {
    let table_id = WIPICTableId::try_from(id.0 >> 16)?;
    let function_id = id.0 as u16;
    let (_, lr) = core.read_pc_lr()?;
    if table_id == WIPICTableId::Kernel && function_id == WIPICKernelMethodId::Reserved1 as u16 {
        return interface::get_wipic_interfaces(core, &mut KtfWIPICContext::new(core.clone(), system.clone(), jvm.clone()))
            .await?
            .write(core, lr);
    }

    let body = method_table::get_method_body(table_id, function_id)
        .ok_or_else(|| WieError::FatalError(alloc::format!("Unknown KTF WIPIC SVC id {:#x}", id.0)))?;

    let mut args = [0; 9];
    for (index, arg) in args.iter_mut().enumerate() {
        *arg = core.read_param(index)?;
    }
    let mut context = KtfWIPICContext::new(core.clone(), system.clone(), jvm.clone());
    let result = body.call(&mut context, &args).await?;
    core.write_return_value(&result.results)?;
    core.set_next_pc(lr)
}

pub fn register_wipic_svc_handler(core: &mut ArmCore, system: &System, jvm: &Jvm) -> Result<()> {
    core.register_svc_handler(SVC_CATEGORY_WIPIC, handle_wipic_svc, &(system.clone(), jvm.clone()))
}
