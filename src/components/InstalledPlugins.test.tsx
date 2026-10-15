import {fireEvent,render,screen,waitFor} from '@testing-library/react';
import {expect,it,vi} from 'vitest';
import {invoke} from '@tauri-apps/api/core';
import InstalledPlugins from './InstalledPlugins';
vi.mock('@tauri-apps/api/core',()=>({invoke:vi.fn()}));
vi.mock('../api/projects',()=>({listProjects:vi.fn().mockResolvedValue([{id:'p',name:'Project',workspace_path:'D:/project',is_archived:0,folders:[{path:'D:/project',trusted:true}]}])}));
it('requires a project and one-use execution approval, exposes revoke while running',async()=>{
 let finish:(v:unknown)=>void=()=>{};
 vi.mocked(invoke).mockImplementation(async(_cmd,args)=>{const request=(args as {request:{action:string}}).request;if(request.action==='plugins')return [{id:'fixture',version:'1.0.0',description:'fixture',enabled:true,commands:['echo']}];if(request.action==='plugin_run')return new Promise(resolve=>{finish=resolve;});return {};});
 render(<InstalledPlugins/>);const run=await screen.findByText('执行 echo');expect(run).toBeDisabled();
 await screen.findByText('Project');fireEvent.change(screen.getByLabelText('插件运行项目'),{target:{value:'p'}});expect(run).toBeDisabled();
 fireEvent.click(screen.getByRole('checkbox'));fireEvent.click(run);
 await waitFor(()=>expect(invoke).toHaveBeenCalledWith('peripheral_command',{request:{action:'plugin_run',id:'fixture',command:'echo',input:{},project_id:'p',confirmed:true}}));
 expect(screen.getByRole('checkbox')).not.toBeChecked();expect(screen.getByText('禁用并停止')).toBeEnabled();fireEvent.click(screen.getByText('禁用并停止'));
 await waitFor(()=>expect(invoke).toHaveBeenCalledWith('peripheral_command',{request:{action:'plugin_enable',id:'fixture',enabled:false}}));finish({stdout:'ok',stderr:''});await screen.findByRole('status');expect(run).toBeDisabled();
});
