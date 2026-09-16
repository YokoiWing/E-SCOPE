/* mockturtle: C++ logic network library
 * Copyright (C) 2018-2022  EPFL
 *
 * Permission is hereby granted, free of charge, to any person
 * obtaining a copy of this software and associated documentation
 * files (the "Software"), to deal in the Software without
 * restriction, including without limitation the rights to use,
 * copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the
 * Software is furnished to do so, subject to the following
 * conditions:
 *
 * The above copyright notice and this permission notice shall be
 * included in all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
 * EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES
 * OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
 * NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT
 * HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
 * WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
 * FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
 * OTHER DEALINGS IN THE SOFTWARE.
 */

/*!
  \file spfd_utils.hpp
  \brief Utilities for spfd-manipulation

  \author Andrea Costamagna
*/

#pragma once

#include "index_list.hpp"
#include <kitty/constructors.hpp>
#include <kitty/operators.hpp>
#include <kitty/print.hpp>
#include <bit>

namespace mockturtle
{

  std::mt19937 RNG(5);

  struct index_with_cost_t
  {
    index_with_cost_t( uint32_t index, uint32_t cost ): index(index), cost(cost){}

    bool operator<( index_with_cost_t const& other ) const
    {
      return cost < other.cost;
    }

    bool operator>( index_with_cost_t const& other ) const
    {
      return cost > other.cost;
    }

    uint32_t index;
    uint32_t cost;


  };

  template<class TT, uint32_t CAP>
  struct spfd_covering_manager_t
  {
    spfd_covering_manager_t(){}
    
    void init( TT const& target, TT const& careset )
    {
      care = careset;
      safe_care = careset;
      func[1] =  target & care;
      func[0] = ~target & care;
      rmnd[1] = func[1];
      rmnd[0] = func[0];
      reset();
    }

    void init( TT const& target )
    {
      care = ~target.construct();
      care = care | ~care;
      safe_care = care;
      func[1] =  target & care;
      func[0] = ~target & care;
      reset();
    }

    void reset()
    {
      masks[0] = safe_care;
      nMasks = 1;
      nEdges = kitty::count_ones( func[1] ) * kitty::count_ones( func[0] );  
      killed[0] = nEdges > 0 ? false : true;
      nKills = nEdges > 0 ? 0 : 1;
      remind = 0;
    }

    void reset( TT const& modified_care, bool complement )
    {
      masks[0] = complement ? safe_care & ~modified_care : safe_care & modified_care;
      nMasks = 1;
      nEdges = kitty::count_ones( func[1] & masks[0] ) * kitty::count_ones( func[0] & masks[0] );  
      killed[0] = nEdges > 0 ? false : true;
      nKills = nEdges > 0 ? 0 : 1;
      remind=0;
    }

    bool update( TT const& tt )
    {
      nEdges = 0;
      for( uint32_t iMask{0}; iMask < nMasks; ++iMask )
      {
        if( killed[iMask] )
        {
          killed[nMasks+iMask] = true;
          nKills++;
        }
        else
        {
          masks[nMasks+iMask] = masks[iMask] & tt;
          masks[iMask] &= ~tt;

          if( ( kitty::count_ones( masks[nMasks+iMask] & func[1] ) == 0 ) || ( kitty::count_ones( masks[nMasks+iMask] & func[0] ) == 0 ) )
          {
            killed[nMasks+iMask] = true;
            nKills++;
          }
          else
          {
            killed[nMasks+iMask] = false;
            nEdges += kitty::count_ones( func[1] & masks[nMasks+iMask] ) * kitty::count_ones( func[0] & masks[nMasks+iMask] );  
          }

          if( kitty::count_ones( masks[iMask] & func[1] ) == 0 || kitty::count_ones( masks[iMask] & func[0] ) == 0 )
          {
            killed[iMask] = true;
            nKills++;
          }
          else
          {
            killed[iMask] = false;
            nEdges += kitty::count_ones( func[1] & masks[iMask] ) * kitty::count_ones( func[0] & masks[iMask] );  
          }
        }
      }
      nMasks = nMasks * 2;
      return true;
    }

    uint32_t evaluate( TT const& tt )
    {
      uint32_t res=0;
      for( auto iMask{0}; iMask<nMasks; ++iMask )
      {
        if( !killed[iMask] )
        {
          res+= kitty::count_ones( func[1] & masks[iMask] &  tt ) * kitty::count_ones( func[0] & masks[iMask] & tt );  
          res+= kitty::count_ones( func[1] & masks[iMask] & ~tt ) * kitty::count_ones( func[0] & masks[iMask] & ~tt );  
        }
      }
      return res;
    } 

    uint32_t evaluate( TT const& tt1, TT const& tt2 )
    {
      uint32_t res=0;
      for( auto iMask{0}; iMask<nMasks; ++iMask )
      {
        if( !killed[iMask] )
        {
          res+= kitty::count_ones( func[1] & masks[iMask] &  tt1 & tt2 ) * kitty::count_ones( func[0] & masks[iMask] & tt1 & tt2 );  
          res+= kitty::count_ones( func[1] & masks[iMask] & ~tt1 & tt2 ) * kitty::count_ones( func[0] & masks[iMask] & ~tt1 & tt2 );  
          res+= kitty::count_ones( func[1] & masks[iMask] & ~tt1 &~tt2 ) * kitty::count_ones( func[0] & masks[iMask] & ~tt1 & ~tt2 );  
          res+= kitty::count_ones( func[1] & masks[iMask] & tt1 & ~tt2 ) * kitty::count_ones( func[0] & masks[iMask] & tt1 &~tt2 );  
        }
      }
      return res;
    } 

    uint32_t evaluate( TT const& tt1, TT const& tt2, TT const& tt3 )
    {
      uint32_t res=0;
      for( auto iMask{0}; iMask<nMasks; ++iMask )
      {
        if( !killed[iMask] )
        {
          res+= kitty::count_ones( func[1] & masks[iMask] &  tt1 &  tt2 &  tt3 ) * kitty::count_ones( func[0] & masks[iMask] &  tt1 &  tt2 &  tt3 );  
          res+= kitty::count_ones( func[1] & masks[iMask] & ~tt1 &  tt2 &  tt3 ) * kitty::count_ones( func[0] & masks[iMask] & ~tt1 &  tt2 &  tt3 );  
          res+= kitty::count_ones( func[1] & masks[iMask] & ~tt1 & ~tt2 &  tt3 ) * kitty::count_ones( func[0] & masks[iMask] & ~tt1 & ~tt2 &  tt3 );  
          res+= kitty::count_ones( func[1] & masks[iMask] &  tt1 & ~tt2 &  tt3 ) * kitty::count_ones( func[0] & masks[iMask] &  tt1 & ~tt2 &  tt3 );  
          res+= kitty::count_ones( func[1] & masks[iMask] &  tt1 &  tt2 & ~tt3 ) * kitty::count_ones( func[0] & masks[iMask] &  tt1 &  tt2 & ~tt3 );  
          res+= kitty::count_ones( func[1] & masks[iMask] & ~tt1 &  tt2 & ~tt3 ) * kitty::count_ones( func[0] & masks[iMask] & ~tt1 &  tt2 & ~tt3 );  
          res+= kitty::count_ones( func[1] & masks[iMask] & ~tt1 & ~tt2 & ~tt3 ) * kitty::count_ones( func[0] & masks[iMask] & ~tt1 & ~tt2 & ~tt3 );  
          res+= kitty::count_ones( func[1] & masks[iMask] &  tt1 & ~tt2 & ~tt3 ) * kitty::count_ones( func[0] & masks[iMask] &  tt1 & ~tt2 & ~tt3 );  
        }
      }
      return res;
    } 

    bool is_covered()
    {
      //printf("%d %d\n", nMasks, nKills);
      return nMasks <= nKills;
    }

    bool is_saturated()
    {
      return nMasks >= CAP;
    }

    uint32_t get_best_reminder()
    {
      std::vector<uint32_t> order;
      std::vector<uint32_t> costs;

      uint32_t best_reminder{0};
      TT mk = care.construct();

      for( auto iMask{0}; iMask<nMasks; ++iMask )
      {
        if( !killed[iMask] )
        {
          mk |= masks[iMask];
        }
      }

      TT tt = func[1].construct();

      int j{0};
      for( int i{0}; i<nMasks; ++i )
      {
        if( !killed[i] )
        {
          if( kitty::count_ones(masks[i] & func[1]) > kitty::count_ones(masks[i] & func[0]) )
          {
            best_reminder |= ( 1u << j );
          }
          j++;
        }
      }

      return best_reminder;
    }

    uint32_t get_best_reminder2()
    {
      rmnd[1] = func[1];
      rmnd[0] = func[0];
      std::vector<uint32_t> order;
      std::vector<uint32_t> costs;

      uint32_t best_reminder{0};
      TT mk = care.construct();

      for( auto iMask{0}; iMask<nMasks; ++iMask )
      {
        if( !killed[iMask] )
        {
          mk |= masks[iMask];
        }
      }

      TT tt = func[1].construct();

      int j{0};
      for( int i{0}; i<nMasks; ++i )
      {
        if( !killed[i] )
        {
          if( kitty::count_ones(masks[i] & func[1]) > kitty::count_ones(masks[i] & func[0]) )
          {
            best_reminder |= ( 1u << j );
            rmnd[0] ^= masks[i];
            rmnd[1] ^= masks[i];
          }
          index_with_cost_t iwcost( j, kitty::count_ones(masks[i])/2-std::min( kitty::count_ones(masks[i] & func[1]), kitty::count_ones(masks[i] & func[0]) ) );
          _indices_with_cost.push_back( iwcost );
          j++;
        }
      }

      std::sort( _indices_with_cost.begin(), _indices_with_cost.end() );
      
      return best_reminder;
    }

    std::tuple<TT, TT> extract_reminder()
    {
      TT mk = care.construct();

      for( auto iMask{0}; iMask<nMasks; ++iMask )
      {
        if( !killed[iMask] )
        {
          mk |= masks[iMask];
        }
      }

      TT tt = func[1].construct();

      int j{0};
      for( int i{0}; i<nMasks; ++i )
      {
        if( !killed[i] )
        {
          if( ( ( remind >> j ) & 0x1 ) == 0x1 )
          {
            j++;
            tt |= masks[i] & func[0];
          }
          else
          {
            j++;
            tt |= masks[i] & func[1];
          }
        }
      }

      remind = (remind + 1)%( 1u << (nMasks-nKills) );
      return std::make_tuple( tt, mk);
    }

    std::tuple<TT, TT> extract_reminder2()
    {
      TT mk = care.construct();

      for( auto iMask{0}; iMask<nMasks; ++iMask )
      {
        if( !killed[iMask] )
        {
          mk |= masks[iMask];
        }
      }

      TT tt = rmnd[1].construct();

      int j{0};
      for( int i{0}; i<nMasks; ++i )
      {
        if( !killed[i] )
        {
          if( ( ( remind >> _indices_with_cost[j].index ) & 0x1 ) == 0x1 )
          {
            j++;
            tt |= masks[i] & rmnd[0];
          }
          else
          {
            j++;
            tt |= masks[i] & rmnd[1];
          }
        }
      }

      remind = (remind + 1)%( 1u << (nMasks-nKills) );
      return std::make_tuple( tt, mk);
    }

    void print()
    {
      for( int i{0}; i<nMasks; ++i )
      {
        if( !killed[i] )
        {
          printf("%2d|", i);

          for( int b{func[1].num_bits()-1}; b>=0; --b )
          {
            if( kitty::get_bit(masks[i],b) == 0 )
            {
              printf("*");
            }
            else
            {
              if( kitty::get_bit(func[1],b) == 0 )
              {
                printf("0");
              }
              else
                printf("1");
            }
          }
          printf("\n");
        }
      }
    }

    std::array<TT, CAP> masks;
    std::array<bool, CAP> killed;
    uint32_t nMasks;
    uint32_t nKills;
    uint32_t nEdges;
    TT care;
    TT safe_care;
    std::array<TT, 2u> func;
    std::array<TT, 2u> rmnd;
    std::vector<index_with_cost_t> _indices_with_cost;

    uint32_t remind{0};
  };

  template<uint32_t NUM_FANINS, uint32_t MAX_WIDTH, class TT = kitty::dynamic_truth_table>
  struct lut_resynthesis_t
  {
    using truth_table_t = TT;
    lut_resynthesis_t()
    {}

    std::vector<uint32_t> find_support( truth_table_t tt, truth_table_t mk )
    {
      std::vector<uint32_t> supp;
      for( int i{0}; i<sim_target.num_vars(); ++i )
      {
        auto tt1 = kitty::cofactor1( tt, i );
        auto tt0 = kitty::cofactor0( tt, i );
        auto mk1 = kitty::cofactor1( mk, i );
        auto mk0 = kitty::cofactor0( mk, i );
        if( !kitty::equal( tt1&mk0&mk1, tt0&mk0&mk1 ) )
        {
          supp.push_back( i );
        }
        else
        {
          tt = tt1&mk1 | tt0&mk0;
          mk = mk1 | mk0;
        }
      }
      //std::sort(supp.begin(), supp.end());
      return supp;
    }

    std::vector<uint32_t> find_supports( truth_table_t tt, truth_table_t mk )
    {
      std::vector<uint32_t> supp;

      for( int i{0}; i<sim_target.num_vars(); ++i )
      {
        auto tt1 = kitty::cofactor1( tt, i );
        auto tt0 = kitty::cofactor0( tt, i );
        auto mk1 = kitty::cofactor1( mk, i );
        auto mk0 = kitty::cofactor0( mk, i );
        if( !kitty::equal( tt1&mk0&mk1, tt0&mk0&mk1 ) )
        {
          supp.push_back( i );
        }
        else
        {
          tt = tt1&mk1 | tt0&mk0;
          mk = mk1 | mk0;
        }
      }
      return supp;
    }

    std::tuple<truth_table_t, truth_table_t> extract_lut( std::vector<uint32_t> const& cut, truth_table_t const& tt, truth_table_t const& mk )
    {
      assert( cut.size() <= NUM_FANINS && "Netlist size exceeds maximum fanin size" );
      truth_table_t lut(cut.size());
      auto sim = tt.construct();
      auto tmp = tt.construct();
      std::uniform_real_distribution<> U01(0, 1);

      for( uint32_t m{0}; m < ( 1u << cut.size() ); ++m )
      {
        tmp = tmp | ~tmp;
        for( int i{0}; i<cut.size(); ++i )
        {
          if( ( ( m >> i ) & 0x1 ) == 0x1 )
          {
            tmp &= sims[cut[i]];
          }
          else
          {
            tmp &= ~sims[cut[i]];
          }  
        }
        int n0 = kitty::count_ones( ~tt & mk & tmp );
        int n1 = kitty::count_ones(  tt & mk & tmp );
        if( n0 < n1 )
        {
          kitty::set_bit( lut, m );
          sim |= tmp;
        }
        else if( n0 > n1 )
        {
          kitty::clear_bit( lut, m );
        }
        else
        {
          if( U01(RNG) < 0.5 )
          {
            kitty::clear_bit( lut, m );
          }
          else
          {
            kitty::set_bit( lut, m );
            sim |= tmp;
          }
        }
      }
      return std::make_tuple( lut, sim );
    } 

    struct comb_t
    {
      comb_t( int N, int K ): N(N), K(K), comb(K)
      {
        for( int j{0}; j<K; ++j )
        {
          comb[j]=j;
        }
        act=K-1;
      }

      std::optional<std::vector<uint32_t>> get()
      {
        bool finished=true;
        for( int i{0}; i<K; ++i )
        {
          if( comb[i]+K <= N+i )
          {
            finished=false;
            break;
          }
        }

        if( finished )
        {
          return std::nullopt;
        }
        auto res = comb;

        for( int j{act}; j>=0; --j )
        {
          comb[j]=comb[j]+1;
          if( comb[j]+K-1 < N+j )
          {
            for( int k{j+1}; k<=act; ++k )
            {
              comb[k]=comb[k-1]+1;
            }
            break;
          }
        }

        return res;
      }

      int N;
      int K;
      std::vector<uint32_t> comb;
      uint32_t act;
    };

    void sort_nlist_by_I( std::vector<uint32_t> & nlist, uint32_t act, truth_table_t const& tt, truth_table_t const& mk )
    { 
      uint32_t best, cost, best_cost;
      manager.init( tt, mk );
      for( int i{0}; i<act; ++i )
      {
        best_cost = std::numeric_limits<uint32_t>::max();

        for( uint32_t j{i}; j<nlist.size(); ++j )
        {
          cost = manager.evaluate( sims[nlist[j]] );
          if( cost < best_cost  )
          {
            best_cost = cost;
            best = j;
          }
        }
        uint32_t tmp = nlist[i];
        nlist[i]=nlist[best];
        nlist[best]=tmp;

        manager.update( sims[nlist[i]] );
      }
    } 

    uint32_t _1decompose( std::vector<uint32_t> const& supp, truth_table_t const& tt, truth_table_t const& mk )
    {
      auto [lut,sim] = extract_lut( supp, tt, mk );

      funcs.push_back( lut );
      supps.push_back( supp );
      sims.push_back( sim );
      return sims.size()-1;
    }

    void clear()
    {
      sims.clear();
      supps.clear();
      funcs.clear();
      manager.reset();
    }

    void reset()
    {
      for( int i{sims.size()-1}; i>=sim_target.num_vars(); --i )
      {
        funcs.erase( funcs.begin() + funcs.size() -1 );
        supps.erase( supps.begin() + supps.size() -1 );
        sims.erase( sims.begin() + sims.size() -1 );
      }
      manager.reset();
    }

    std::optional<uint32_t> _2decompose( std::vector<uint32_t>& supp, truth_table_t const& tt, truth_table_t const& mk )
    {
      if( KILLER > _num_inserts ) return std::nullopt;
      sort_nlist_by_I( supp, supp.size(), tt, mk );
      {
        comb_t combs( supp.size(), NUM_FANINS -1 );
        std::vector<uint32_t> free_supp;
        
        int it{0};
        while( true )
        {
          auto comb = combs.get();
          if( !comb )
          {
            return std::nullopt;
          }

          free_supp.clear();
          manager.init( tt, mk );

          for( int i{0}; i<(*comb).size(); ++i )
          {
            free_supp.push_back( supp[(*comb)[i]] );
            manager.update( sims[supp[(*comb)[i]]] );
          }

          
          manager.remind = manager.get_best_reminder();

          int extreme = int(1+_effort*(1u<<(manager.nMasks-manager.nKills-1)));
          for( int m{0}; m<extreme; ++m )
          {
            auto [tt_new, mk_new] = manager.extract_reminder();

            auto supp_bound = find_support( tt_new, mk_new );

            if( supp_bound.size() > NUM_FANINS )
              continue;

            uint32_t lit_b;
            if( supp_bound.size() <= NUM_FANINS )
            {
              auto [lut,sim] = extract_lut( supp_bound, tt_new, mk_new );
              funcs.push_back( lut );
              supps.push_back( supp_bound );
              sims.push_back( sim );
              free_supp.push_back( sims.size()-1 );

              auto [lut_f,sim_f] = extract_lut( free_supp, tt, mk );
              funcs.push_back( lut_f );
              supps.push_back( free_supp );
              sims.push_back( sim_f );

              return sims.size()-1;

            }
          }
        }
      }
      return std::nullopt;
    }

    std::optional<uint32_t> _Kdecompose( std::vector<uint32_t>& supp, truth_table_t const& tt, truth_table_t const& mk )
    {
      if( KILLER > _num_inserts ) return std::nullopt;
      sort_nlist_by_I( supp, supp.size(), tt, mk );

      std::vector<uint32_t> supp_f={supp[0]};
    //  for( int i{0}; i<NUM_FANINS-2; ++i )
    //  {
    //    supp_f.push_back( supp[0] );
    //  }

      auto tt1 = kitty::cofactor1( tt, supp[0] );
      auto mk1 = kitty::cofactor1( mk, supp[0] );
//      manager.init( tt1, mk1 );
//      for( int i{1}; i<supp_f.size(); ++i )
//      {
//        manager.update( sims[supp_f[i]] );
//      }
//      auto [tt1_r, mk1_r] = manager.extract_reminder();
      auto res1 = decompose_rec( tt1, mk1 );
      if( !res1 )
      {
        reset();
        return std::nullopt;
      }

      auto tt0 = kitty::cofactor0( tt, supp[0] );
      auto mk0 = kitty::cofactor0( mk, supp[0] );
//      manager.init( tt0, mk0 );
//      for( int i{1}; i<supp_f.size(); ++i )
//      {
//        manager.update( sims[supp_f[i]] );
//      }
//      auto [tt0_r, mk0_r] = manager.extract_reminder();
      auto res0 = decompose_rec( tt0, mk0 );
      if( !res0 )
      {
        reset();
        return std::nullopt;
      }
      supp_f.push_back( *res1 );
      supp_f.push_back( *res0 );

      auto [lut,sim] = extract_lut( supp_f, tt, mk );
      
      funcs.push_back( lut );
      supps.push_back( supp_f );
      sims.push_back( sim );
      return sims.size()-1;
    }

    std::optional<uint32_t> _Tdecompose( std::vector<uint32_t>& supp, truth_table_t const& tt, truth_table_t const& mk )
    {
      if( KILLER > _num_inserts ) return std::nullopt;
      bool upd{true};

      truth_table_t tt_r = tt;
      truth_table_t mk_r = mk;
      std::vector<uint32_t> supp_f;

      while( upd && ( supp_f.size() < NUM_FANINS - 1 ) )
      {
        upd=false;
        for( int i{0}; i<supp.size(); ++i )
        {
          if( std::find( supp_f.begin(), supp_f.end(), supp[i] ) != supp_f.end() )
            continue;

          auto tt0 = kitty::cofactor0( tt_r&mk, i );
          auto tt1 = kitty::cofactor1( tt_r&mk, i );
          auto mk0 = kitty::cofactor0( mk, i );
          auto mk1 = kitty::cofactor1( mk, i );

          if( kitty::is_const0( tt0 ) )
          {
            upd = true;
            tt_r = tt1;
            supp_f.push_back( supp[i] );
            break;
          }
          else if( kitty::is_const0( tt1 ) )
          {
            upd = true;
            tt_r = tt0;
            supp_f.push_back( supp[i] );
            break;
          }
          else if( kitty::equal( tt1&mk1, mk1 ) )
          {
            upd = true;
            tt_r = tt0;
            supp_f.push_back( supp[i] );
            break;
          }
          else if( kitty::equal( tt0&mk0, mk0 ) )
          {
            upd = true;
            tt_r = tt1;
            supp_f.push_back( supp[i] );
            break;
          }
          else if( kitty::equal( (~tt1)&mk0&mk1, tt0&mk0&mk1 ) )
          {
            upd = true;
            tt_r = (tt0 & mk0) | (~tt1&mk1);
            mk_r = mk0 | mk1;
            supp_f.push_back( supp[i] );
            break;
          }
        }
      }

      if( supp_f.size() > 0 )
      {
        auto lit_r = decompose_rec( tt_r, mk_r );
        if( !lit_r )
          return std::nullopt;

        supp_f.push_back(*lit_r);
        auto [lut,sim] = extract_lut( supp_f, tt, mk );
        if( kitty::equal( sim&mk, tt&mk ) )
        {
          funcs.push_back( lut );
          supps.push_back( supp_f );
          sims.push_back( sim );
          return sims.size()-1;
        }
        else
        {
          reset();
          return std::nullopt;
        }
      }

      return std::nullopt;
    }

    std::optional<uint32_t> decompose_rec( truth_table_t const& tt, truth_table_t const& mk )
    {      
      KILLER++;
      if( KILLER > _num_inserts ) return std::nullopt;
      auto supp = find_support( tt, mk );

      /* 0-resynthesis */
      if( supp.size() == 1 )
      {
        return supp[0];
      }
      /* 1-resynthesis */
      if( supp.size() <= NUM_FANINS )
      {
        return _1decompose( supp, tt, mk );
      }

      /* 2-resynthesis */
      if( supp.size() <= 2*NUM_FANINS-1 )
      {
        auto lit2 = _2decompose( supp, tt, mk );

        if( lit2 )
          return *lit2;
      }

      auto litT = _Tdecompose( supp, tt, mk );

      if( litT )
      {
        return *litT;
      }



      auto litK = _Kdecompose( supp, tt, mk ); 

      if( litK )
        return *litK;
        
      return std::nullopt;

    }

    std::optional<uint32_t> decompose( truth_table_t const& tt, truth_table_t const& mk, uint32_t num_inserts, double effort=1 )
    {
      assert(effort <= 1 );
      KILLER=0;
      _num_inserts = num_inserts;
      _effort = effort;

      /* initialization */
      sims.clear();
      sim_target = tt;

      funcs.clear();
      supps.clear();

      for( int i{0}; i<tt.num_vars(); ++i )
      {
        funcs.emplace_back(1u);
        supps.push_back({i});
        sims.emplace_back(tt.num_vars());
        kitty::create_nth_var( sims[i], i );
      }

      return decompose_rec( tt, mk );

    }

    std::optional<uint32_t> decompose( truth_table_t const& tt, uint32_t num_inserts, double effort=1 )
    {
      _num_inserts = num_inserts;
      _effort = effort;
      auto mk = ~tt.construct();
      return decompose( tt, mk, num_inserts, effort );
    }

    uint32_t num_luts()
    {
        return (uint32_t)funcs.size() - sim_target.num_vars();
    }

    template<class Ntk>
    signal<Ntk> add_to_network( Ntk& ntk, std::vector<node<Ntk>> nodes )
    {
      signal<Ntk> res = 0;
      std::vector<signal<Ntk>> children;
      std::vector<signal<Ntk>> supp;
      for( auto s : nodes )
        supp.push_back( ntk.make_signal(s) );

      for( int i{sim_target.num_vars()}; i<sims.size(); ++i )
      {
        children.clear();
        for( auto x : supps[i] )
        {
          children.push_back( supp[x] );
        }
        res = ntk.create_node( children, funcs[i] );
        supp.push_back( res );
      }

      return res;
    }

    void print()
    {
        for( int i{0}; i<sims.size(); ++i )
        {
            printf( "%3d ", i );
            kitty::print_binary( sims[i] );
            printf("\n");
        }

        printf( "    " );
        kitty::print_binary( sim_target );
        printf("\n");
    }

    large_lig_index_list::element_type to_index_list( large_lig_index_list& index_list, std::vector<large_lig_index_list::element_type> lits )
    {
      std::vector<large_lig_index_list::element_type> supp;
      large_lig_index_list::element_type lit;

      for( int i{sim_target.num_vars()}; i<sims.size(); ++i )
      {
        supp.clear();
        for( auto x : supps[i] )
        {
          supp.push_back( lits[x] );
        }
        lit = index_list.add_function( supp, funcs[i] );
        lits.push_back( lit );
      }

      return lit;
    }

    public:
        truth_table_t sim_target;
        spfd_covering_manager_t<truth_table_t, 1u << MAX_WIDTH> manager;

        std::vector<std::vector<uint32_t>> supps;
        std::vector<truth_table_t> funcs;
        std::vector<truth_table_t> sims;
        uint32_t KILLER{0};

        uint32_t _num_inserts;
        double _effort;

  };




  struct scored_div
  {
    scored_div( uint32_t l, uint32_t s )
        : div( l ), score( s )
    {}

    bool operator==( scored_div const& other ) const
    {
      return div == other.div;
    }

    bool operator<( scored_div const& other ) const
    {
      return score < other.score;
    }

    bool operator>( scored_div const& other ) const
    {
      return score > other.score;
    }

    uint32_t div;
    uint32_t score;
  };

  struct fscored_div
  {
    fscored_div( uint32_t l, double s )
        : div( l ), score( s )
    {}

    bool operator==( fscored_div const& other ) const
    {
      return div == other.div;
    }

    bool operator<( fscored_div const& other ) const
    {
      return score < other.score;
    }

    bool operator>( fscored_div const& other ) const
    {
      return score > other.score;
    }

    uint32_t div;
    double score;
  };

  enum support_selection_t
  {
    RND,
    GRE,
    PV1,
    PV2,
    PV3,
    ENU,
    EX1,
    EX2,
    EX3
  };

template<class TT, class Ntk, uint32_t IGCAP>
class support_selector
{

public:
  using truth_table_t = TT;
  using truth_tableK_t = kitty::static_truth_table<IGCAP>;
  using node_type = typename Ntk::node;
  using truth_table_storage_type = incomplete_node_map<TT, Ntk>;

public:
  explicit support_selector( support_selection_t algo = GRE, uint32_t max_support_size = IGCAP ) : _algo(algo), _max_support_size( max_support_size )
  {
    assert( max_support_size <= IGCAP );
    divisors.reserve( 200u );
    RNG.seed( 5 );
    _status = {0,1,2,3,4,5};
  }

  template<class iterator_type>
  std::optional<std::vector<uint32_t>> operator()( TT const& target, TT const& care, iterator_type begin, iterator_type end, truth_table_storage_type const& tts )
  {
    static_assert( std::is_same_v<typename std::iterator_traits<iterator_type>::value_type, node_type>, "iterator_type does not dereference to node_type" );

    ptts = &tts;
    on_off_sets[0] = ~target & care;
    on_off_sets[1] = target & care;

    if( kitty::count_ones(on_off_sets[0]) == 0 || kitty::count_ones(on_off_sets[1]) == 0 )
    {
      return std::nullopt;
    }

    _igraph.init( target, care );

    divisors.clear();
    scored_divs.clear();

    while ( begin != end )
    {
      divisors.emplace_back( *begin );
      scored_divs.emplace_back( divisors.size() - 1, _igraph.evaluate( get_div( divisors.size() - 1 ) ) );
      ++begin;
    }

    std::sort( scored_divs.begin(), scored_divs.end() );
    for( auto sdiv : scored_divs )
    {
      if( sdiv.score == 0 )
      {
        return std::nullopt;
      }
    }

    if( _algo == support_selection_t::ENU )
    {
      return try_enum2();
    }
    else if( _algo == support_selection_t::RND )
    {
      for( int j{0}; j < nIters; ++j )
      {
        auto res = try_random();
        if( res )
        {
          return *res;
        }
      }
    }
    else if( _algo == support_selection_t::GRE )
    {
      for( int j{0}; j < nIters; ++j )
      {
        auto res = try_greedy();
        if( res )
        {
          return *res;
        }
      }
    }
    else if( _algo == support_selection_t::EX1 )
    {
      for( int j{0}; j < nIters; ++j )
      {
        auto res = try_exp<1>();
        if( res )
        {
          return *res;
        }
      }
    }
    else if( _algo == support_selection_t::EX2 )
    {
      for( int j{0}; j < nIters; ++j )
      {
        auto res = try_exp<2>();
        if( res )
        {
          return *res;
        }
      }
    }
    else if( _algo == support_selection_t::EX3 )
    {
      for( int j{0}; j < nIters; ++j )
      {
        auto res = try_exp<3>();
        if( res )
        {
          return *res;
        }
      }
    }
    else if( _algo == support_selection_t::PV1 )
    {
      return try_piv<1>();
    }
    else if( _algo == support_selection_t::PV2 )
    {
      return try_piv<2>();
    }
    else if( _algo == support_selection_t::PV3 )
    {
      return try_piv<3>();
    }

    return std::nullopt;
  }

  /* given the current path, it finds the next valid path 
   * that could give a correct result 
   */
  bool update_path( std::vector<uint32_t> & path )
  {

    /* loop next */
    while( path.size() <= std::min(_max_support_size, (uint32_t)divisors.size()) )
    {
      _igraph.reset();
      uint32_t level = path.size()-1;
      for( int j{level}; j>=0; j-- )
      {
        if( path[j]<(divisors.size()-1-(level-j)) )
        {
          path[j]++;
          for( int k{j+1}; k<path.size(); ++k )
          {
            path[k]=path[k-1]+1;
          }
          /* should I try it? */
          int ubound = _igraph.nEdges;
          for( auto x : path )
          {
            ubound -= (_igraph.nEdges - scored_divs[x].score);
          }
          /* increase the size of the path or return false */
          if( (ubound <= 0 ) )
          {
            return true;
          }
          else
          {
            break;
          }
        }
      }

      if( path.size() == _max_support_size )
      {
        return false;
      }
      /* time to go to the next level */
      for( int i{0}; i<path.size(); ++i )
      {
        path[i] = i;
      }
      if( path.size() == 0 )
        path={0};
      else
        path.push_back( path.back() + 1 );
      
      /* should I try it? */
      uint32_t ubound = 0;
      for( auto x : path )
      {
        ubound += scored_divs[x].score;
      }

      /* increase the size of the path or return false */
      if( (ubound < _igraph.nEdges) )
      {
        return true;
      }
    }
    /* check coverage bound */
    return false;
  }

  /* enumerate all possible solutions and return if you find one */
  std::optional<std::vector<uint32_t>> try_branch_and_bound()
  {
    std::vector<uint32_t> path;
    while( update_path( path ) )
    {
      _igraph.reset();
      for( auto x : path )
      {
        _igraph.update( get_div( scored_divs[x].div ) );
      }

      if( _igraph.is_covered() )
      {
        std::vector<uint32_t> supp;
        for( auto x : path )
        {
          supp.push_back( scored_divs[x].div );
        }
        _igraph.reset();
        int idx=0;

        for (int i = 0; i < supp.size(); i++)
        {
          auto reward = _igraph.evaluate( get_div( supp[i] ) );


          for (int j = scored_divs.size()-1; j >= 0; --j)
          {
            if( scored_divs[j].score == reward )
            {
              idx = j;
            }
          }
          _igraph.update( get_div(supp[i]) );

          for (int j = scored_divs.size()-1; j >= 0; --j)
          {
            scored_divs[j].score = _igraph.evaluate( get_div(scored_divs[j].div) );
          }

          std::sort( scored_divs.begin(), scored_divs.end() );
        }
        
        return supp;
      }
    }
    return std::nullopt;
  }

  std::optional<std::vector<uint32_t>> try_greedy( uint32_t start = 0, std::vector<uint32_t> supp0 = {} )
  {
    uint32_t cost, best_cost;
    std::vector<uint32_t> best_candidates;
    std::vector<uint32_t> supp;

    _igraph.reset();
    for ( auto x : supp0 )
    {
      _igraph.update( get_div( x ) );
      supp.push_back( x );
    }

    /* add recomputation of the support */
    while ( !_igraph.is_covered() && supp.size() < _max_support_size )
    {
      best_cost = std::numeric_limits<uint32_t>::max();
      if ( _igraph.is_saturated() )
        break;
      for ( uint32_t iCnd{ start }; iCnd < scored_divs.size(); ++iCnd )
      {
        cost = _igraph.evaluate( get_div( scored_divs[iCnd].div ) );
        if ( cost < best_cost )
        {
          best_cost = cost;
          best_candidates = { scored_divs[iCnd].div };
        }
        else if ( cost == best_cost )
        {
          best_candidates.push_back( scored_divs[iCnd].div );
        }
      }
      if ( best_candidates.size() == 0 )
        break;

      std::uniform_int_distribution<> distrib( 0, best_candidates.size() - 1 );
      int idx = distrib( RNG );
      supp.push_back( best_candidates[idx] );
      _igraph.update( get_div( best_candidates[idx] ) );
    }

    if ( _igraph.is_covered() && supp.size() <= _max_support_size )
    {
      std::sort( supp.begin(), supp.end() );

      return supp;
    }
    return std::nullopt;
  }

  std::optional<std::vector<uint32_t>> try_enum2()
  {

    static constexpr bool verbose=false;
    static constexpr bool verbose2=false;
    std::shuffle( scored_divs.begin(), scored_divs.end(), RNG );

    _results.clear();
    _igraph.reset();
    if( _igraph.nEdges == 0 || _igraph.is_covered() ) 
      return std::nullopt;

    if( divisors.size() < 3 ) return std::nullopt;
    /* 2 size support */
    for( int i{ 0 }; i < scored_divs.size(); ++i )
    {
      for( int j{ i+1 }; j < scored_divs.size(); ++j )
      {
        _igraph.reset();

        uint32_t nEdges = _igraph.nEdges;
        _igraph.update(get_div(scored_divs[i].div));
        if( _igraph.is_covered() || nEdges <= _igraph.nEdges )  continue;

        _igraph.update(get_div(scored_divs[j].div));
        if( _igraph.is_covered() )
        {
          auto res = std::vector{scored_divs[i].div, scored_divs[j].div};

          if constexpr ( verbose )
          {
            uint32_t H;
            uint32_t Hmin;
            uint32_t Hmax;
            uint32_t Hpre;

            _igraph.reset();
            
            for( int q{0}; q<res.size(); ++q )
            {
              Hmax = std::numeric_limits<uint32_t>::min();
              Hmin = std::numeric_limits<uint32_t>::max();
              if( _igraph.is_covered() )
                break;
              Hpre = _igraph.nEdges;
              for( int i{ 0 }; i < scored_divs.size(); ++i )
              {
                H = _igraph.evaluate( get_div(scored_divs[i].div) );
                if( H < Hmin && H < Hpre )
                {
                  Hmin = H;
                }
                if( H > Hmax && H < Hpre )
                {
                  Hmax = H;
                }
              }

              uint32_t x;
              uint32_t hmin = std::numeric_limits<uint32_t>::max();
              for( auto xc : res )
              {
                H = _igraph.evaluate( get_div(xc) );
                if( H < hmin )
                {
                  x=xc;
                  hmin=H;
                }
              }

              H = hmin;
              printf("%d %d %d %d ", H, Hmin, Hmax, Hpre);

              Hpre = H;
              _igraph.update( get_div(x) );
            }
          }

          if constexpr ( verbose2 )
          {
            uint32_t H;
            uint32_t Hmin;
            uint32_t Hmax;
            uint32_t Hpre;

            _igraph.reset();
            
            for( int q{0}; q<res.size(); ++q )
            {
              Hmax = std::numeric_limits<uint32_t>::min();
              Hmin = std::numeric_limits<uint32_t>::max();
              if( _igraph.is_covered() )
                break;
              Hpre = _igraph.nEdges;
              for( int i{ 0 }; i < scored_divs.size(); ++i )
              {
                H = _igraph.evaluate( get_div(scored_divs[i].div) );
                if( H < Hmin && H < Hpre )
                {
                  Hmin = H;
                }
                if( H > Hmax && H < Hpre )
                {
                  Hmax = H;
                }
              }

              for( int i{ 0 }; i < scored_divs.size(); ++i )
              {
                //if( std::find( res.begin(), res.end(), scored_divs[i].div ) == res.end() )
                {
                  double hd = 1.0*_igraph.evaluate( get_div(scored_divs[i].div) );
                  hd -= 1.0*Hmin;
                  hd = hd / ( 1.0*( Hpre- Hmin ) );
                  printf("%f ", hd );
                }
              }

              //printf("%d %d %d %d ", H, Hmin, Hmax, Hpre);
              uint32_t x;
              uint32_t hmin = std::numeric_limits<uint32_t>::max();
              for( auto xc : res )
              {
                H = _igraph.evaluate( get_div(xc) );
                if( H < hmin )
                {
                  x=xc;
                  hmin=H;
                }
              }
              Hpre = H;
              _igraph.update( get_div(x) );
            }
          }


          std::sort(res.begin(), res.end());
          return res;
        }

        if( divisors.size() < 4 ) continue;
        for( int k{ j+1 }; k < scored_divs.size(); ++k )
        {
          _igraph.reset();

          uint32_t nEdges = _igraph.nEdges;
          _igraph.update(get_div(scored_divs[i].div));
          if( _igraph.is_covered() || nEdges <= _igraph.nEdges )  continue;

          nEdges = _igraph.nEdges;
          _igraph.update(get_div(scored_divs[j].div));
          if( _igraph.is_covered() || nEdges <= _igraph.nEdges )  continue;

          _igraph.update(get_div(scored_divs[k].div));
          if( _igraph.is_covered() )
          {
            //printf("[%d %d %d]", scored_divs[i].div,scored_divs[j].div,scored_divs[k].div );
            auto res = std::vector{scored_divs[i].div, scored_divs[j].div, scored_divs[k].div};

            if constexpr ( verbose )
            {
              uint32_t H;
              uint32_t Hmin;
              uint32_t Hmax;
              uint32_t Hpre;

              _igraph.reset();
              
              for( int q{0}; q<res.size(); ++q )
              {
                Hmax = std::numeric_limits<uint32_t>::min();
                Hmin = std::numeric_limits<uint32_t>::max();
                if( _igraph.is_covered() )
                  break;
                Hpre = _igraph.nEdges;
                for( int i{ 0 }; i < scored_divs.size(); ++i )
                {
                  H = _igraph.evaluate( get_div(scored_divs[i].div) );
                  if( H < Hmin && H < Hpre )
                  {
                    Hmin = H;
                  }
                  if( H > Hmax && H < Hpre )
                  {
                    Hmax = H;
                  }
                }

                uint32_t x;
                uint32_t hmin = std::numeric_limits<uint32_t>::max();
                for( auto xc : res )
                {
                  H = _igraph.evaluate( get_div(xc) );
                  if( H < hmin )
                  {
                    x=xc;
                    hmin=H;
                  }
                }

                H = hmin;
                printf("%d %d %d %d ", H, Hmin, Hmax, Hpre);

                Hpre = H;
                _igraph.update( get_div(x) );
              }
            }

          if constexpr ( verbose2 )
          {
            uint32_t H;
            uint32_t Hmin;
            uint32_t Hmax;
            uint32_t Hpre;

            _igraph.reset();
            
            for( int q{0}; q<res.size(); ++q )
            {
              Hmax = std::numeric_limits<uint32_t>::min();
              Hmin = std::numeric_limits<uint32_t>::max();
              if( _igraph.is_covered() )
                break;
              Hpre = _igraph.nEdges;
              for( int i{ 0 }; i < scored_divs.size(); ++i )
              {
                H = _igraph.evaluate( get_div(scored_divs[i].div) );
                if( H < Hmin && H < Hpre )
                {
                  Hmin = H;
                }
                if( H > Hmax && H < Hpre )
                {
                  Hmax = H;
                }
              }

              for( int i{ 0 }; i < scored_divs.size(); ++i )
              {
                //if( std::find( res.begin(), res.end(), scored_divs[i].div ) == res.end() )
                {
                  double hd = 1.0*_igraph.evaluate( get_div(scored_divs[i].div) );
                  hd -= 1.0*Hmin;
                  hd = hd / ( 1.0*( Hpre- Hmin ) );
                  printf("%f ", hd );
                }
              }

              //printf("%d %d %d %d ", H, Hmin, Hmax, Hpre);
              uint32_t x;
              uint32_t hmin = std::numeric_limits<uint32_t>::max();
              for( auto xc : res )
              {
                H = _igraph.evaluate( get_div(scored_divs[i].div) );
                if( H < hmin )
                {
                  x=xc;
                  hmin=H;
                }
              }
              Hpre = H;
              _igraph.update( get_div(x) );
            }
          }

            std::sort(res.begin(), res.end());
            return res;
            //_results.push_back( std::vector{scored_divs[i].div,scored_divs[j].div,scored_divs[k].div} );
          }


          if( divisors.size() < 5 ) continue;
          for( int l{ k+1 }; l < scored_divs.size(); ++l )
          {
            _igraph.reset();

            uint32_t nEdges = _igraph.nEdges;
            _igraph.update(get_div(scored_divs[i].div));
            if( _igraph.is_covered() || nEdges <= _igraph.nEdges )  continue;

            nEdges = _igraph.nEdges;
            _igraph.update(get_div(scored_divs[j].div));
            if( _igraph.is_covered() || nEdges <= _igraph.nEdges )  continue;

            nEdges = _igraph.nEdges;
            _igraph.update(get_div(scored_divs[k].div));
            if( _igraph.is_covered() || nEdges <= _igraph.nEdges )  continue;

            _igraph.update(get_div(scored_divs[l].div));
            if( _igraph.is_covered() )
            {
              //printf("[%d %d %d]", scored_divs[i].div,scored_divs[j].div,scored_divs[k].div );
              auto res = std::vector{scored_divs[i].div, scored_divs[j].div, scored_divs[k].div, scored_divs[l].div};


            if constexpr( verbose )
            {
              uint32_t H;
              uint32_t Hmin;
              uint32_t Hmax;
              uint32_t Hpre;

              _igraph.reset();
              
              for( int q{0}; q<res.size(); ++q )
              {
                Hmax = std::numeric_limits<uint32_t>::min();
                Hmin = std::numeric_limits<uint32_t>::max();
                if( _igraph.is_covered() )
                  break;
                Hpre = _igraph.nEdges;
                for( int i{ 0 }; i < scored_divs.size(); ++i )
                {
                  H = _igraph.evaluate( get_div(scored_divs[i].div) );
                  if( H < Hmin && H < Hpre )
                  {
                    Hmin = H;
                  }
                  if( H > Hmax && H < Hpre )
                  {
                    Hmax = H;
                  }
                }

                uint32_t x;
                uint32_t hmin = std::numeric_limits<uint32_t>::max();
                for( auto xc : res )
                {
                  H = _igraph.evaluate( get_div(xc) );
                  if( H < hmin )
                  {
                    x=xc;
                    hmin=H;
                  }
                }

                H = hmin;
                printf("%d %d %d %d ", H, Hmin, Hmax, Hpre);

                Hpre = H;
                _igraph.update( get_div(x) );
              }
            }

          if constexpr ( verbose2 )
          {
            uint32_t H;
            uint32_t Hmin;
            uint32_t Hmax;
            uint32_t Hpre;

            _igraph.reset();
            
            for( int q{0}; q<res.size(); ++q )
            {
              Hmax = std::numeric_limits<uint32_t>::min();
              Hmin = std::numeric_limits<uint32_t>::max();
              if( _igraph.is_covered() )
                break;
              Hpre = _igraph.nEdges;
              for( int i{ 0 }; i < scored_divs.size(); ++i )
              {
                H = _igraph.evaluate( get_div(scored_divs[i].div) );
                if( H < Hmin && H < Hpre )
                {
                  Hmin = H;
                }
                if( H > Hmax && H < Hpre )
                {
                  Hmax = H;
                }
              }

              for( int i{ 0 }; i < scored_divs.size(); ++i )
              {
                //if( std::find( res.begin(), res.end(), scored_divs[i].div ) == res.end() )
                {
                  double hd = 1.0*_igraph.evaluate( get_div(scored_divs[i].div) );
                  hd -= 1.0*Hmin;
                  hd = hd / ( 1.0*( Hpre- Hmin ) );
                  printf("%f ", hd );
                }
              }

              //printf("%d %d %d %d ", H, Hmin, Hmax, Hpre);
              uint32_t x;
              uint32_t hmin = std::numeric_limits<uint32_t>::max();
              for( auto xc : res )
              {
                H = _igraph.evaluate( get_div(xc) );
                if( H < hmin )
                {
                  x=xc;
                  hmin=H;
                }
              }
              Hpre = H;
              _igraph.update( get_div(x) );
            }
          }

              std::sort(res.begin(), res.end());
              return res;
              //_results.push_back( std::vector{scored_divs[i].div,scored_divs[j].div,scored_divs[k].div} );
            }
          } 

        } 
      } 
    }

    return std::nullopt;
  }

  std::optional<std::vector<uint32_t>> try_enum( uint32_t start = 0, std::vector<uint32_t> supp0 = {} )
  {
    std::shuffle( scored_divs.begin(), scored_divs.end(), RNG );
    _results.clear();
    _igraph.reset();
    //printf("try enum %d\n", _igraph.nEdges);
    if( _igraph.nEdges == 0 || _igraph.is_covered() ) 
      return std::nullopt;

    /* 2 size support */
    if( scored_divs.size() > 2 )
    {
      for( int i{ 0 }; i < scored_divs.size()-1; ++i )
      {
        for( int j{ i+1 }; j < scored_divs.size(); ++j )
        {
          //printf("P2|E| %d\n", _igraph.nEdges );
          _igraph.reset();
          //printf("Q2|E| %d\n", _igraph.nEdges );
          if( scored_divs[i].score+scored_divs[j].score > _igraph.nEdges )
          {
            break;
          }
          uint32_t nEdges = _igraph.nEdges;
          _igraph.update(get_div(scored_divs[i].div));
          if( _igraph.is_covered() || nEdges <= _igraph.nEdges )  continue;

          nEdges = _igraph.nEdges;
          _igraph.update(get_div(scored_divs[j].div));
          if( _igraph.is_covered() )
          {
            //printf("[%d %d]", scored_divs[i].div,scored_divs[j].div );
            auto res = std::vector{scored_divs[i], scored_divs[j]};
            std::sort(res.begin(), res.end());
            return std::vector{res[0].div, res[1].div};
            _results.push_back( std::vector{scored_divs[i].div,scored_divs[j].div} );
          }
        } 
      }
    }

    _igraph.reset();

    if( scored_divs.size() > 3 )
    {
      /* 2 size support */
      for( int i{ 0 }; i < scored_divs.size()-2; ++i )
      {
        for( int j{ i+1 }; j < scored_divs.size()-1; ++j )
        {
          for( int k{ j+1 }; k < scored_divs.size(); ++k )
          {
            //printf("P3|E| %d\n", _igraph.nEdges );
            _igraph.reset();
            //printf("Q3|E| %d\n", _igraph.nEdges );
            if( scored_divs[i].score+scored_divs[j].score+scored_divs[k].score > 2*_igraph.nEdges )
            {
              break;
            }
            uint32_t nEdges = _igraph.nEdges;
            _igraph.update(get_div(scored_divs[i].div));
            if( _igraph.is_covered() || nEdges <= _igraph.nEdges )  continue;

            nEdges = _igraph.nEdges;
            _igraph.update(get_div(scored_divs[j].div));
            if( _igraph.is_covered() || nEdges <= _igraph.nEdges )  continue;

            _igraph.update(get_div(scored_divs[k].div));
            if( _igraph.is_covered() )
            {
              //printf("[%d %d %d]", scored_divs[i].div,scored_divs[j].div,scored_divs[k].div );
              auto res = std::vector{scored_divs[i], scored_divs[j], scored_divs[k]};
              std::sort(res.begin(), res.end());
              return std::vector{res[0].div, res[1].div, res[2].div};
              _results.push_back( std::vector{scored_divs[i].div,scored_divs[j].div,scored_divs[k].div} );
            }
          } 
        } 
      }
    }

    uint32_t H;
    uint32_t Hmin;
    uint32_t Hmax;
    uint32_t Hpre;

    for( auto res : _results )
    {
      _igraph.reset();
      //printf("O|E| %d\n", _igraph.nEdges );
      Hmax = std::numeric_limits<uint32_t>::min();
      Hmin = std::numeric_limits<uint32_t>::max();
      
      for( auto x : res )
      {
        Hpre = _igraph.nEdges;
        for( int i{ 0 }; i < scored_divs.size(); ++i )
        {
          //kitty::print_binary( get_div(scored_divs[i].div) );

          H = _igraph.evaluate( get_div(scored_divs[i].div) );
          if( H < Hmin && H < Hpre )
          {
            Hmin = H;
          }
          if( H > Hmax && H < Hpre )
          {
            Hmax = H;
          }
        }

        H = _igraph.evaluate( get_div(x) );
        //printf("%d %d %d %d ", H, Hmin, Hmax, Hpre);

        Hpre = H;
        _igraph.update( get_div(x) );

      }
      //printf("\n");
    }

    return std::nullopt;
  }


  bool recursive_enum3( std::vector<uint32_t> & supp )
  {
    if( supp.size() > _max_support_size )
    {
      return false;
    }
    _igraph.reset();
    uint32_t Enew, Eold = _igraph.nEdges;
    for( int s{0}; s<supp.size(); ++s )
    {
      _igraph.update( get_div(scored_divs[s].div) );
      Enew = _igraph.nEdges;
      if( Enew == Eold )
      {
        return false;
      }
    }
    if( _igraph.is_covered() )
    {
      return true;
    }

    for( uint32_t d{supp.back()+1}; d<scored_divs.size(); ++d )
    {
      supp.push_back( d );
      bool found = recursive_enum3( supp );
      if( found ) 
      {
        return true;
      }
      supp.erase( supp.begin() + supp.size() - 1 );
    }
    return false;
  }

  std::optional<std::vector<uint32_t>> try_enum3( )
  {

    std::shuffle( scored_divs.begin(), scored_divs.end(), RNG );

    _igraph.reset();
    if( _igraph.nEdges == 0 || _igraph.is_covered() ) 
      return std::nullopt;

    for( uint32_t d{0}; d<scored_divs.size(); ++d )
    {
      std::vector<uint32_t> isupp = {d};
      bool found = recursive_enum3( isupp );
      if( found ) 
      {
        std::vector<uint32_t> supp;
        for( auto s : isupp )
        {
          supp.push_back( scored_divs[s].div );
        }
        return supp;
      }
    }

    return std::nullopt;
  }



  std::optional<std::vector<uint32_t>> try_rgreedy( uint32_t start = 0, std::vector<uint32_t> supp0 = {} )
  {
    //printf("w\n");
    uint32_t cost, best_cost;
    std::vector<uint32_t> best_candidates;
    std::vector<uint32_t> supp;
    auto sdivs = scored_divs;
    _igraph.reset();
    for ( auto x : supp0 )
    {
      _igraph.update( get_div( x ) );
      supp.push_back( x );
    }

    /* add recomputation of the support */
    while ( !_igraph.is_covered() && supp.size() < _max_support_size )
    {
      best_cost = std::numeric_limits<uint32_t>::max();
      if ( _igraph.is_saturated() )
        break;

      double mean=0;
      for ( uint32_t iCnd{ 0 }; iCnd < sdivs.size(); ++iCnd )
      {
        sdivs[iCnd].score = _igraph.evaluate( get_div( sdivs[iCnd].div ) );
        if( sdivs[iCnd].score != _igraph.nEdges )
          mean+=sdivs[iCnd].score;
      }
      mean /= (sdivs.size()-supp.size());
      //printf("mean %f\n", mean);
      if(mean < 1) mean = 1.0;

      std::vector<double> cdfs={0};
      for ( uint32_t iCnd{ 0 }; iCnd < sdivs.size(); ++iCnd )
      {
        cdfs.push_back( exp(-1.0*sdivs[iCnd].score/mean)/mean );
      }

      for ( uint32_t i{ 1 }; i < cdfs.size(); ++i )
      {
        cdfs[i]+=cdfs[i-1];
      }

      for ( uint32_t i{ 0 }; i < cdfs.size(); ++i )
      {
        cdfs[i]/=cdfs.back();
        //printf("%f ", cdfs[i]);
      }
      //printf("\n");
      std::uniform_real_distribution<> distrib( 0, 0.9999 );
      bool done=false;

      while( !done )
      {
        double alpha = distrib( RNG );
        uint32_t dbest=sdivs[0].div;
        for ( int iCnd{ cdfs.size()-1}; iCnd >=0 ; --iCnd )
        {
          //printf("%f<?<%f\n", cdfs[iCnd], alpha );
          if( cdfs[iCnd]<alpha )
          {
            if( std::find( supp.begin(), supp.end(), sdivs[iCnd].div ) == supp.end() )
            {
              done = true;
              dbest=sdivs[iCnd].div;
              //printf("*_*%d\n", dbest);
              supp.push_back( dbest );
              _igraph.update( get_div( dbest ) );
              break;
            }
          } 
        }
      }
    }

    if ( _igraph.is_covered() && supp.size() <= _max_support_size )
    {
      std::sort( supp.begin(), supp.end() );

      return supp;
    }
    return std::nullopt;
  }

  std::optional<std::vector<uint32_t>> try_random( uint32_t start = 0, std::vector<uint32_t> supp0 = {} )
  {
    std::vector<uint32_t> supp;

    _igraph.reset();
    for ( auto x : supp0 )
    {
      _igraph.update( get_div( x ) );
      supp.push_back( x );
    }

    while ( !_igraph.is_covered() && supp.size() < _max_support_size )
    {
      if ( _igraph.is_saturated() )
        break;

      std::uniform_int_distribution<> distrib( 0, divisors.size()-1 );
      bool done=false;

      int it{0};
      while( it++<10 )
      {
        int cand = distrib( RNG );
        if( std::find( supp.begin(), supp.end(), cand ) == supp.end() )
        {
          supp.push_back( cand );
          _igraph.update( get_div( cand ) );
          break;
        }
      }
      if( it == 10 )
      {
        //printf("[w] runtime for choosing divisor exceeded\n");
        return std::nullopt;
      }
    }

    if ( _igraph.is_covered() && supp.size() <= _max_support_size )
    {
      std::sort( supp.begin(), supp.end() );

      return supp;
    }
    return std::nullopt;
  }

  template<uint32_t FN>
  std::optional<std::vector<uint32_t>> try_piv()
  {
    
    std::vector<fscored_div> fscored_divs;
    for( int s = 0; s < scored_divs.size(); ++s )
    {
      fscored_divs.emplace_back(scored_divs[s].div, fitted<FN>(scored_divs[s].score)); 
    }
    std::sort( fscored_divs.begin(), fscored_divs.end() );

    int i=0;
    for( int s = 0; s < nIters; ++s )
    {
      _igraph.reset();
      uint32_t div = fscored_divs[s%fscored_divs.size()].div;
      auto suppj = try_exp<FN>(0,{div});
      if( suppj )
        return *suppj;
    }
    //printf("end\n");
    return std::nullopt;
  }

    
  template<uint32_t FN, class Xt>
  double fitted( Xt x )
  {
    if constexpr( FN == 1 )
    {
      double A { 25.74487865 };
      double B { 208.63234918 };
      return A*exp(-B*x);
    }
    else if constexpr( FN == 2 )
    {
      double A1 { 23.41130732 };
      double A2 { 2.3672926 };
      double B1 { 380.33272547 };
      double B2 { 8.64283253 };
      return A1 * exp(-B1 * x ) + A2*exp(-B2 * x);
    }
    else if constexpr( FN == 3 )
    {
      double A1 { 2.80922094 };
      double A2 { 22.9693746 };
      double A3 { 0.316818737 };
      double A4 { 0.446865505 };
      double B1 { 12.1699240 };
      double B2 { 472.527419 };
      return A1 * exp(-B1 * x)  + A2 * exp(-B2*x)  + A3*exp(- (x-0.32)*(x-0.32)/(2*0.001))  +  A4*exp(- (x-0.18)*(x-0.18)/(2*0.0005));
    }
    else
    {
      printf("[e] unknown functionality %d\n", FN );
    }
  }

  template<uint32_t FN>
  std::optional<std::vector<uint32_t>> try_exp( uint32_t start = 0, std::vector<uint32_t> supp0 = {} )
  {

    if( divisors.size() <= _max_support_size ) return std::nullopt;
    double H, Hmin, Hpre;
    Hmin = std::numeric_limits<double>::max();
    std::vector<uint32_t> supp;
    _igraph.reset();
    std::vector<double> P;

    for ( auto x : supp0 )
    {
      _igraph.update( get_div( x ) );
      supp.push_back( x );
    }

    /* add recomputation of the support */
    while ( !_igraph.is_covered() && supp.size() < _max_support_size )
    {
      Hpre=(double)_igraph.nEdges;
      if ( _igraph.is_saturated() )
        break;

      P={0};
      for ( uint32_t d{ 0 }; d < divisors.size(); ++d )
      {
        H = (double)_igraph.evaluate( get_div( d ) );
        if( H < Hmin )
        {
          Hmin=H;
        }
        P.push_back(H);        
      }
      double eps = 0.000001;
      for ( uint32_t d{ 0 }; d < divisors.size(); ++d )
      {
        double cost = (P[d+1]-Hmin)/(Hpre-Hmin+eps);
        P[d+1] = fitted<FN>(cost); 
        //printf("cost=%f->P[%d/%d]=%f ", cost, d, divisors.size(), P[d+1]); 
      }
        //printf("\n"); 


      for ( uint32_t d{ 0 }; d < divisors.size(); ++d )
      {
        P[d+1] += P[d];
      }

      for ( uint32_t d{ 0 }; d < divisors.size(); ++d )
      {
        P[d+1] /= P[divisors.size()];
      }

      std::uniform_real_distribution<> distrib( 0, 0.9999 );
      bool done=false;

      int it{0};
      while( it++<10 && !done )
      {
        double alpha = distrib( RNG );
        //printf("alpha=%f\n", alpha );
        for ( int d{ P.size()-1}; d >=0 ; --d )
        {
          //printf("%f<?%f\n", P[d], alpha );
          if( P[d]<alpha )
          {
            if( std::find( supp.begin(), supp.end(), d ) == supp.end() )
            {
              done = true;
              supp.push_back( d );
              _igraph.update( get_div( d ) );
              break;
            }
          } 
        }
      }
      if( it >= 10 )
      {
      //  printf("[w] exceeded runtime for sampling\n");
        return std::nullopt;
      }
    }

    if ( _igraph.is_covered() && supp.size() <= _max_support_size )
    {
      std::sort( supp.begin(), supp.end() );

      return supp;
    }
    return std::nullopt;
  }


  inline TT const& get_div( uint32_t idx ) const
  {
    return ( *ptts )[divisors[idx]];
  }


  private:
    std::array<TT, 2> on_off_sets;
    std::array<uint32_t, 2> num_bits; /* number of bits in on-set and off-set */

    const truth_table_storage_type* ptts;
    std::vector<node_type> divisors;

    spfd_covering_manager_t<truth_table_t, 1 << IGCAP> _igraph;
    uint32_t nMax = std::numeric_limits<uint32_t>::max();
    uint32_t nIters=10;
    uint32_t _max_support_size;
    std::vector<scored_div> scored_divs;
    support_selection_t _algo;
  public:
    std::vector<std::vector<uint32_t>> _results;
    std::vector<uint32_t> _status{0,0,0,0,0,0};

};

} // namespace mockturtle